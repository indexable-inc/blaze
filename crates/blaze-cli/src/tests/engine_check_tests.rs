//! `blaze engine-check` against `blaze check` on one checkout (engine.md E6): what the engine
//! path prints is what the old machine prints for the stages the engine runs. A child of
//! main.rs's tests: its helpers are theirs.
//!
//! Today the engine runs the outlines alone, so the oracle is: every finding `engine-check`
//! lists is one `check` lists (by code and id), the engine path lists at least one (the planted
//! file-level syntax error), and the engine entered each planned task once.

use super::*;

/// A crate root, two modules (one nested), and a stray `}` in `src/a/b.rs`: a file-level
/// syntax error in a file the walk reaches two `mod`s down.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", "mod a;\n\npub fn top() -> u32 {\n    1\n}\n");
    write(io, "src/a.rs", "pub mod b;\n\npub fn mid() -> u32 {\n    2\n}\n");
    write(io, "src/a/b.rs", "pub fn low() -> u32 {\n    3\n}\n}\n");
}

/// What `engine-check` listed (each finding's code and id) and its counts.
fn engine(io: &Io, workers: u32) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let err = io.need_err().expect("err");
    let producers = blaze_ids::producer::Producers::for_tests(b"test");
    let opts = engine_check::Opts {
        tools: engine_store::tools(&home::tools(&producers, lints::Lints::NONE.rules)),
        workers: core::num::NonZeroU32::new(workers).expect("a worker"),
        toolchain: shelf(io),
        heap: blaze_own::testing::heap(),
        // As the tests' `check` runs (main.rs `ask_framed`).
        place: engine_check::Place {
            checkout: "ws",
            cert_file: None,
            cert_dir: None,
            home: Some("/h"),
            cargo_home: None,
            start: Some("/"),
            sdk: crate::sdk::NONE_IN_TESTS,
            exe_dir: Err(fetch::ExeDirWhy::NotRead),
        },
    };
    let checked = engine_check::check(&cx, io, &root, opts, &mut shown, &mut phases, &out, &err).expect("engine-check answers");
    let ids = shown.listed.iter().map(|l| (l.code().to_owned(), l.id.as_str().to_owned())).collect();
    (ids, checked)
}

/// Every phase entered each task it planned, once.
fn entered_once(checked: &engine_check::Checked, workers: u32) {
    assert!(!checked.phases.is_empty(), "no phase ran with {workers} worker(s)");
    for (phase, planned, entered) in checked.phases.iter() {
        assert_eq!(planned, entered, "{phase:?} with {workers} worker(s): tasks planned and entered");
    }
    assert_eq!(checked.tasks, checked.entered, "tasks planned and entered with {workers} worker(s)");
}

/// Two members, `b` reading `a` through one glob and one import `a` does not declare.
fn two_crates(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "pub mod m;\n\npub fn f() -> u8 {\n    1\n}\n");
    write(io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", "use a::*;\nuse a::g;\n\npub fn h() -> u8 {\n    f() + m::k()\n}\n");
}

/// The names stage on the engine (engine.md E6, second step): `engine-check` resolves every
/// crate of the unit graph (the two members and the shelf's crates), each `Names` task entered
/// once at its crate's depth, and lists the names findings `check` lists: the one unresolved
/// import, and nothing for the glob. 1 and 4 workers give the same answer. CONTROL: before the
/// names stage, `engine-check` listed no `unresolved` finding here.
#[test]
fn engine_check_resolves_the_names_check_resolves() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    two_crates(&io);
    let old = ask(&io, (Command::Check, "check"), work::Sizes::USUAL).expect("check answers");
    let unresolved = |ids: &[(std::string::String, std::string::String)]| -> std::vec::Vec<(std::string::String, std::string::String)> {
        let mut found: std::vec::Vec<_> = ids.iter().filter(|(code, _)| code == "unresolved").cloned().collect();
        found.sort();
        found
    };
    let expected = unresolved(&old.ids);
    assert_eq!(expected.len(), 1, "check lists the one unresolved import: {:?}", old.ids);
    let mut answers = std::vec::Vec::new();
    for workers in [1, 4] {
        let (ids, checked) = engine(&io, workers);
        assert_eq!(unresolved(&ids), expected, "engine-check with {workers} worker(s) lists check's names findings; it listed {ids:?}");
        for id in &ids {
            assert!(old.ids.contains(id), "engine-check listed {id:?}, which check did not: check listed {:?}", old.ids);
        }
        entered_once(&checked, workers);
        let names: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Names).map(|(phase, planned, _)| (phase.wave, *planned)).collect();
        assert!(names.len() >= 2, "the crates' names run in more than one wave (b after a): {names:?}");
        assert!(checked.puts > 0, "the phases' records were put to the store");
        // No answer is kept between commands yet: every entry is new, none the same bytes again,
        // and the table's counts are the entries.
        assert!(checked.because.iter().all(|(group, _)| group.cause == "new" && group.definer.is_none()), "{:?}", checked.because);
        assert_eq!(checked.because.iter().map(|(_, count)| count).sum::<u32>(), checked.entered, "the table counts every entry");
        let names_entered: u32 = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Names).map(|(_, _, entered)| entered).sum();
        assert_eq!(checked.stages[blaze_engine::Stage::Names.index()], blaze_engine::StageCount { entered: names_entered, identical: 0 });
        println!("engine-check names with {workers} worker(s): phases {:?}, puts {}", checked.phases.iter().map(|(phase, planned, entered)| (*phase, *planned, *entered)).collect::<std::vec::Vec<_>>(), checked.puts);
        answers.push((ids, checked.phases.iter().copied().collect::<std::vec::Vec<_>>()));
    }
    assert_eq!(answers[0], answers[1], "1 and 4 workers give the same answer");
}

#[test]
fn engine_check_lists_what_check_lists_for_the_outlines() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    workspace(&io);
    let old = ask(&io, (Command::Check, "check"), work::Sizes::USUAL).expect("check answers");
    for workers in [1, 4] {
        let (ids, checked) = engine(&io, workers);
        assert!(!ids.is_empty(), "engine-check with {workers} worker(s) listed nothing; check listed {:?}", old.ids);
        for id in &ids {
            assert!(old.ids.contains(id), "engine-check listed {id:?}, which check did not: check listed {:?}", old.ids);
        }
        // One `Outlines` task a file (lib.rs, a.rs, a/b.rs), each entered once; every phase
        // entered what it planned.
        let outlines: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Outlines).map(|(_, planned, entered)| (*planned, *entered)).collect();
        assert_eq!(outlines, [(3, 3)], "outline tasks planned and entered with {workers} worker(s)");
        entered_once(&checked, workers);
        assert_eq!(checked.status.0, Status::FAILED.0, "a syntax error fails the command");
    }
}
