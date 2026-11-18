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
    engine_from(io, workers, true)
}

/// The command's options as the tests' `check` runs (main.rs `ask_framed`), on the checkout `ws`.
/// Its request frame is leaked: a cap whose blocks are taken as used (engine_check.rs
/// `REQUEST_FRAME`), one a call, as `engine_check::serve` makes one a request.
fn opts(io: &Io, workers: u32, fresh: bool) -> engine_check::Opts<'static> {
    let frame: &'static bstd::mem::Frame = std::boxed::Box::leak(std::boxed::Box::new(bstd::mem::Frame::with_reserve(engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("the request's frame")));
    let producers = blaze_ids::producer::Producers::for_tests(b"test");
    engine_check::Opts {
        tools: engine_store::tools(&home::tools(&producers, lints::Lints::NONE.rules)),
        workers: core::num::NonZeroU32::new(workers).expect("a worker"),
        toolchain: shelf(io),
        heap: blaze_own::testing::heap(),
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
        fresh,
        frame,
    }
}

/// `blaze why <target>` on the checkout `ws`: what it printed.
fn why(io: &Io, target: &str) -> std::string::String {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    let before = stdout(io).len();
    if let Err(error) = crate::engine_why::explain(&cx, io, &root, &opts(io, 2, false), target, &mut phases, &out, &err) {
        panic!("why {target} answers: {error:?}");
    }
    stdout(io).split_off(before)
}

/// [`engine`], starting from the stored answer unless `fresh`.
fn engine_from(io: &Io, workers: u32, fresh: bool) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let err = io.need_err().expect("err");
    let opts = opts(io, workers, fresh);
    let checked = match engine_check::check(&cx, io, &root, opts, &mut shown, &mut phases, &out, &err) {
        Ok(checked) => checked,
        Err(error) => panic!("engine-check (fresh {fresh}) answers: {error:?}"),
    };
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

/// The Bodies test's three fixtures.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Typed {
    /// `a::one` and `b::h` calling it: every body checks clean.
    Plain,
    /// `a::pick<T: Copy>` and `b::h` calling it: one clean generic fn.
    Generic,
    /// `Plain` and `b::wrong`, whose type is wrong (E0308).
    Wrong,
}

/// Two members, `b` calling into `a`, as `typed` says.
fn typed_crates(io: &Io, typed: Typed) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    let a = match typed {
        Typed::Generic => "pub fn pick<T: Copy>(x: T) -> T {\n    x\n}\n",
        Typed::Plain | Typed::Wrong => "pub fn one() -> u8 {\n    1\n}\n",
    };
    write(io, "a/src/lib.rs", a);
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    let b = match typed {
        Typed::Plain => "pub fn h() -> u8 {\n    a::one() + 1\n}\n",
        Typed::Generic => "pub fn h() -> u8 {\n    a::pick(1u8)\n}\n",
        Typed::Wrong => "pub fn h() -> u8 {\n    a::one() + 1\n}\n\npub fn wrong() -> u8 {\n    true\n}\n",
    };
    write(io, "b/src/lib.rs", b);
}

/// THE BODIES STAGE (engine.md E5/E6): every body is its own `BodyCheck` task, entered once,
/// on 1 and 4 workers with one answer, and the answer is `check`'s: the same exit status and
/// the same word on whether types were checked. The plain pair checks every body clean and
/// says types were checked; the wrong body is its one note and fails the command.
/// CONTROL: before the stage, `engine-check` said "NOT checked: types" on the plain pair, where
/// `check` does not (the types oracle below).
#[test]
fn engine_check_checks_each_body_as_its_own_task() {
    for typed in [Typed::Plain, Typed::Generic, Typed::Wrong] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        typed_crates(&io, typed);
        let old = ask(&io, (Command::Check, "check"), work::Sizes::USUAL).expect("check answers");
        let old_said = stdout(&io);
        let types_not_checked = |said: &str| said.contains("NOT checked: types");
        assert_eq!(old.errors > 0, typed == Typed::Wrong, "check's errors on {typed:?}: {:?}\n{old_said}", old.words);
        let mut answers = std::vec::Vec::new();
        for workers in [1, 4] {
            let before = stdout(&io).len();
            let (ids, checked) = engine(&io, workers);
            let said = stdout(&io).split_off(before);
            entered_once(&checked, workers);
            let bodies: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Bodies).map(|(_, planned, entered)| (*planned, *entered)).collect();
            // A task a body, each entered once: `h` and `one` or `pick`, and `wrong` when written.
            let want = if typed == Typed::Wrong { 3 } else { 2 };
            assert_eq!(bodies, [(want, want)], "Bodies planned and entered with {workers} worker(s) on {typed:?}");
            assert_eq!(checked.stages[blaze_engine::Stage::Bodies.index()].entered, want);
            // A time row a phase, in the phases' order; every Outlines and Bodies task timed.
            let timed: std::vec::Vec<_> = checked.times.iter().map(|one| one.phase).collect();
            let phased: std::vec::Vec<_> = checked.phases.iter().map(|(phase, ..)| *phase).collect();
            assert_eq!(timed, phased, "a time row a phase with {workers} worker(s)");
            for (one, (_, _, entered)) in checked.times.iter().zip(checked.phases.iter()) {
                if matches!(one.phase.stage, blaze_engine::Stage::Outlines | blaze_engine::Stage::Bodies) {
                    assert_eq!(one.timed, *entered, "{:?}: every task timed with {workers} worker(s)", one.phase);
                    assert_eq!(one.slowest.iter().flatten().count(), (*entered as usize).min(crate::engine_times::SLOWEST), "{:?}: its slowest named", one.phase);
                }
            }
            let status = checked.status.0;
            let (notes, clean) = &checked.bodies;
            println!("engine-check bodies with {workers} worker(s) on {typed:?}: {clean} clean, notes {notes:?}, ids {ids:?}");
            assert_eq!(Some(status), old.status, "engine-check's status is check's with {workers} worker(s) on {typed:?}; check said:\n{old_said}\nengine-check said:\n{said}");
            assert_eq!(types_not_checked(&said), types_not_checked(&old_said), "the word on types is check's with {workers} worker(s) on {typed:?}; check said:\n{old_said}\nengine-check said:\n{said}");
            match typed {
                Typed::Plain => {
                    assert!(notes.is_empty(), "the plain pair's bodies have no note with {workers} worker(s): {notes:?}");
                    assert_eq!(*clean, 2, "`one` and `h` checked clean with {workers} worker(s)");
                    assert!(!types_not_checked(&said), "types checked whole on the plain pair:\n{said}");
                }
                Typed::Generic => assert_eq!(notes.len() + *clean as usize, 2, "each body is clean or a note: {notes:?}"),
                Typed::Wrong => {
                    assert_eq!(notes.len(), 1, "the one wrong body is the one note with {workers} worker(s): {notes:?}");
                    assert!(notes[0].contains("wrong"), "the note names `wrong`: {notes:?}");
                    assert_eq!(*clean, 2, "`one` and `h` checked clean with {workers} worker(s)");
                }
            }
            answers.push((ids, status, notes.clone(), checked.phases.iter().copied().collect::<std::vec::Vec<_>>()));
        }
        assert_eq!(answers[0], answers[1], "1 and 4 workers give the same answer on {typed:?}");
    }
}

/// A SIGNATURE EDIT RE-CHECKS ITS CALLERS: from the plain pair's stored answer, `a::one`'s
/// signature is edited so `b::h`'s unchanged body no longer type-checks (its return type, then
/// its parameters). The incremental request must list `h`'s note and answer as a fresh run
/// does: the same status and the same body notes. CONTROL: the stored answer before the edit
/// has no note.
#[test]
fn engine_check_rechecks_the_callers_of_an_edited_signature() {
    // `two` calls `one` in its own file; `b::h` calls it from the other crate.
    let caller = "\n\npub fn two() -> u8 {\n    one() + 1\n}\n";
    for edited in ["pub fn one() -> bool {\n    true\n}", "pub fn one(x: u8) -> u8 {\n    x\n}"] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        typed_crates(&io, Typed::Plain);
        write(&io, "a/src/lib.rs", &format!("pub fn one() -> u8 {{\n    1\n}}{caller}"));
        let (_, first) = engine_from(&io, 2, false);
        assert!(first.bodies.0.is_empty(), "the pair checks clean first: {:?}", first.bodies.0);
        write(&io, "a/src/lib.rs", &format!("{edited}{caller}"));
        let (_, now) = engine_from(&io, 2, false);
        let (_, fresh) = engine_from(&io, 2, true);
        assert_eq!(fresh.bodies.0.len(), 2, "a fresh run finds `two` and `h` wrong after {edited:?}: {:?}", fresh.bodies.0);
        assert_eq!(now.bodies.0, fresh.bodies.0, "the incremental request's body notes are a fresh run's after {edited:?}\n{}", entries(&now));
        assert_eq!(now.status.0, fresh.status.0, "the incremental status is a fresh run's after {edited:?}");
        assert_eq!(now.stages[blaze_engine::Stage::Bodies.index()].entered, 3, "`one`, `two` and `h` entered after {edited:?}\n{}", entries(&now));
    }
}

/// AN EDITION EDIT IS IN THE KEYS (rule 21): `a`'s body names a local `gen`, a name in 2021 and
/// a keyword in 2024. From the stored answer of the 2021 crate, `a/Cargo.toml` alone moves to
/// 2024: no file's text moved, and the incremental request must answer as a fresh run does.
/// CONTROL: the fresh 2024 run differs from the 2021 answer (the edition changes the parse), and
/// before the edition was folded into the outline and body sources (engine_tasks.rs
/// `edition_source`) the outline and body rows stood by their texts' keys.
#[test]
fn engine_check_keys_an_outline_and_its_bodies_by_their_edition() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let a = |edition: &str| format!("[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n");
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", &a("2021"));
    write(&io, "a/src/lib.rs", "pub fn one() -> u8 {\n    let gen = 1u8;\n    gen\n}\n");
    let (old_ids, first) = engine_from(&io, 2, false);
    write(&io, "a/Cargo.toml", &a("2024"));
    let (now_ids, now) = engine_from(&io, 2, false);
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    println!("edition: 2021 {old_ids:?} {:?} {:?}; 2024 now {now_ids:?} {:?} {:?}; fresh {fresh_ids:?} {:?} {:?}", first.status.0, first.bodies.0, now.status.0, now.bodies.0, fresh.status.0, fresh.bodies.0);
    assert!((fresh_ids.clone(), fresh.status.0, fresh.bodies.0.clone()) != (old_ids, first.status.0, first.bodies.0.clone()), "control: the 2024 parse differs from the 2021 one");
    assert_eq!(now_ids, fresh_ids, "the incremental request lists a fresh run's findings after the edition edit\n{}", entries(&now));
    assert_eq!(now.bodies.0, fresh.bodies.0, "the incremental body notes are a fresh run's\n{}", entries(&now));
    assert_eq!(now.status.0, fresh.status.0, "the incremental status is a fresh run's");
    assert!(now.stages[blaze_engine::Stage::Outlines.index()].entered >= 1, "the file is outlined again\n{}", entries(&now));
}

/// A PROJECT-SIZED PLAN (tempo's 16,000 bodies panicked in the body plan, g112128: a list of
/// found bodies grown in the command's fixed 64 MiB frame): one file of `BODIES` one-line fns
/// is planned, entered and checked without a panic, every body its own task. CONTROL: on tree
/// bdb5142c1e the lists in the frame (`Took`, the labels, the puts) grew with the bodies.
#[test]
fn engine_check_plans_a_project_sized_file() {
    const BODIES: usize = 50_000;
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    let mut text = std::string::String::with_capacity(BODIES * 24);
    for at in 0..BODIES {
        text.push_str(&std::format!("pub fn f{at}() -> u8 {{ 1 }}\n"));
    }
    write(&io, "src/lib.rs", &text);
    let began = std::time::Instant::now();
    let (_, checked) = engine(&io, 4);
    println!("engine-check of {BODIES} one-line fns: {:?}, phases {:?}", began.elapsed(), checked.phases);
    entered_once(&checked, 4);
    let bodies: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Bodies).map(|(_, planned, entered)| (*planned, *entered)).collect();
    assert_eq!(bodies, [(BODIES as u32, BODIES as u32)], "a task a body, each entered once");
    let (notes, clean) = &checked.bodies;
    assert_eq!(*clean as usize, BODIES, "every body checked clean; notes: {:?}", &notes[..notes.len().min(3)]);
}

/// A PROJECT-SIZED WORKSPACE (tempo, 1,208 crates, panicked at 95.7 s, g114729: a crate
/// environment's pairs refused by the command's fixed 64 MiB frame, which the stages' lists of
/// crates had filled): `CRATES` member crates, each a lib with two fns and a dependency on the
/// previous three, planned and entered with no panic, every body checked clean. The stages'
/// lists are in the request's frame (engine_check.rs `REQUEST_FRAME`). 400 crates, not 1,500:
/// the simulated world's lock and names of 1,500 crates do not fit the 240 s limit.
#[test]
fn engine_check_plans_a_project_sized_workspace() {
    const CRATES: usize = 400;
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let mut members = std::string::String::new();
    for at in 0..CRATES {
        members.push_str(&std::format!("\"c{at}\", "));
    }
    write(&io, "Cargo.toml", &std::format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"));
    for at in 0..CRATES {
        let mut manifest = std::format!("[package]\nname = \"c{at}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n");
        // `f` calls each dependency's `g` and returns its own `g`'s value: no method, no
        // arithmetic (the simulated shelf's `core` is small).
        let mut calls = std::string::String::new();
        for dep in at.saturating_sub(3)..at {
            manifest.push_str(&std::format!("c{dep} = {{ path = \"../c{dep}\" }}\n"));
            calls.push_str(&std::format!("    let _ = c{dep}::g{dep}();\n"));
        }
        write(&io, &std::format!("c{at}/Cargo.toml"), &manifest);
        write(&io, &std::format!("c{at}/src/lib.rs"), &std::format!("pub fn f{at}() -> u8 {{\n{calls}    g{at}()\n}}\n\npub fn g{at}() -> u8 {{\n    2\n}}\n"));
    }
    let began = std::time::Instant::now();
    let (_, checked) = engine(&io, 4);
    println!("engine-check of {CRATES} crates: {:?}, phases {:?}", began.elapsed(), checked.phases);
    entered_once(&checked, 4);
    let names: u32 = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Names).map(|(_, _, entered)| *entered).sum();
    assert!(names as usize >= CRATES, "a names task a crate: {names}");
    let bodies: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Bodies).map(|(_, planned, entered)| (*planned, *entered)).collect();
    assert_eq!(bodies, [(2 * CRATES as u32, 2 * CRATES as u32)], "a task a body, each entered once");
    let (notes, clean) = &checked.bodies;
    assert_eq!(*clean as usize, 2 * CRATES, "every body checked clean; notes: {:?}", &notes[..notes.len().min(3)]);
}

/// BODIES A MACRO WROTE ARE CHECKED (serde-root printed "an entry a macro takes ... not
/// checked" twice, g112128): an entry a macro call is at is one task checking each body its
/// expansion wrote, as `check` covers them (body_types/covered.rs `written_by`): the written fn
/// checks clean, and its wrong form is the one note and fails the command, as `check` does.
/// CONTROL: before, both forms were "not checked" notes and the command said types incomplete.
#[test]
fn engine_check_checks_the_bodies_a_macro_wrote() {
    let good = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u8 { x }\n    };\n}\nwrite_f!();\n";
    let bad = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u16 { x }\n    };\n}\nwrite_f!();\n";
    for (text, wrong) in [(good, false), (bad, true)] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", text);
        let old = ask(&io, (Command::Check, "check"), work::Sizes::USUAL).expect("check answers");
        let old_said = stdout(&io);
        let before = old_said.len();
        let (_, checked) = engine(&io, 1);
        let said = stdout(&io).split_off(before);
        entered_once(&checked, 1);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of a macro-written fn (wrong {wrong}): {clean} clean, notes {notes:?}\n{said}");
        assert_eq!(Some(checked.status.0), old.status, "engine-check's status is check's (wrong {wrong}); check said:\n{old_said}\nengine-check said:\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked")), "no body left unchecked: {notes:?}");
        assert_eq!(notes.len(), usize::from(wrong), "the wrong written body is the one note: {notes:?}");
        assert!(!said.contains("NOT checked: types"), "types checked whole:\n{said}");
    }
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

/// Each stage's entries and the "entered because" table of one request, for a message.
fn entries(checked: &engine_check::Checked) -> std::string::String {
    let stages: std::vec::Vec<_> = blaze_engine::Stage::ALL.iter().map(|stage| (*stage, checked.stages[stage.index()])).filter(|(_, count)| count.entered > 0).collect();
    let because: std::vec::Vec<_> = checked.because.iter().map(|(group, count)| (group.stage, group.cause, group.definer.map(|name| name.kind), *count)).collect();
    format!(
        "certified {}, from the stored answer {} (unreadable: {:?}), entered {}, stages {stages:?}, because {because:?}",
        checked.certified, checked.from_last, checked.unreadable, checked.entered
    )
}

/// The stored answer (engine.md E6, the incremental half): a second request with nothing
/// changed is the certificate (no task entered); a comment, an item added and a module
/// dropped each enter their file's `Outlines` task and only the readers of what moved; after
/// each, nothing changed is the certificate again. CONTROL: a fresh request (no stored answer
/// read) enters every task.
#[test]
fn engine_check_starts_from_its_stored_answer() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    // `two_crates` without the unresolved import: no findings, so nothing keeps the
    // certificate from standing.
    two_crates(&io);
    write(&io, "b/src/lib.rs", "use a::*;\n\npub fn h() -> u8 {\n    f() + m::k()\n}\n");
    let (ids, first) = engine_from(&io, 2, false);
    assert!(!first.certified && first.entered == first.tasks && first.entered > 0, "the first request enters every task: {}", entries(&first));
    println!("the first request: {}, status {}, bodies {:?}", entries(&first), first.status.0, first.bodies);
    let stage = |checked: &engine_check::Checked, stage: blaze_engine::Stage| checked.stages[stage.index()];
    let nothing = |what: &str| {
        let (again, checked) = engine_from(&io, 2, false);
        println!("nothing changed after {what}: {}", entries(&checked));
        assert!(checked.certified && checked.entered == 0, "nothing changed after {what}: the certificate; {}", entries(&checked));
        assert_eq!(again, ids, "the certificate after {what} prints what the first request printed");
        assert_eq!(checked.status.0, first.status.0, "the certificate after {what} exits as the first request");
    };
    nothing("the first request");
    // CONTROL: a fresh request enters every task again.
    let (_, fresh) = engine_from(&io, 2, true);
    assert_eq!(fresh.entered, first.entered, "a fresh request enters every task: {}", entries(&fresh));
    nothing("a fresh request");

    // Exactly these entered, each for this cause (stage, cause's kind, the moved cell's task).
    let because = |checked: &engine_check::Checked| -> std::vec::Vec<(blaze_engine::Stage, &'static str, Option<blaze_drive::Kind>, u32)> {
        let mut groups: std::vec::Vec<_> = checked.because.iter().map(|(group, count)| (group.stage, group.cause, group.definer.map(|name| name.kind), *count)).collect();
        groups.sort();
        groups
    };
    // A comment at the end of one file: its file's outline alone, named by its path (it stands
    // while its text moves), entered because its key moved; its surface cell stands, so no
    // crate's names and no body is entered.
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n// a comment\n");
    let (_, comment) = engine_from(&io, 2, false);
    assert_eq!(because(&comment), [(blaze_engine::Stage::Outlines, "key moved", None, 1)], "a comment enters its file's outline alone: {}", entries(&comment));
    assert_eq!(comment.entered, 1, "a comment: {}", entries(&comment));
    nothing("a comment");

    // A statement added to one body: that file's outline and that one body, each by its key.
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    let two = 2;\n    two\n}\n// a comment\n");
    let (_, statement) = engine_from(&io, 2, false);
    assert_eq!(
        because(&statement),
        [(blaze_engine::Stage::Outlines, "key moved", None, 1), (blaze_engine::Stage::Bodies, "key moved", None, 1)],
        "a statement enters its outline and its body alone: {}",
        entries(&statement)
    );
    nothing("a statement");

    // An item added: its outline by its key, then each crate's names whose read moved.
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n// a comment\npub fn k2() -> u8 {\n    3\n}\n");
    let (_, item) = engine_from(&io, 2, false);
    assert_eq!(stage(&item, blaze_engine::Stage::Outlines).entered, 1, "an item added: {}", entries(&item));
    assert!(stage(&item, blaze_engine::Stage::Names).entered >= 1, "an item added moves its crate's names: {}", entries(&item));
    assert!(
        item.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Names).all(|(group, _)| group.cause == "read moved"),
        "a crate's names are entered for a read that moved, nothing else: {}",
        entries(&item)
    );
    assert!(item.entered < first.entered, "an item added enters less than the first request: {}", entries(&item));
    println!("an item added: {}", entries(&item));
    nothing("an item added");

    // A module dropped: its parent's outline by its key; the dropped file's row goes.
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "b/src/lib.rs", "use a::*;\n\npub fn h() -> u8 {\n    f()\n}\n");
    let (_, dropped) = engine_from(&io, 2, false);
    assert_eq!(stage(&dropped, blaze_engine::Stage::Outlines).entered, 2, "the two edited files' outlines: {}", entries(&dropped));
    println!("a module dropped: {}", entries(&dropped));
    nothing("a module dropped");
}

/// `blaze why` (engine_why.rs): after an item is added to crate `a`'s root, `why` of `b`'s body
/// `h`, which reads `a`'s names through its glob, prints the chain back to the edit: the body
/// entered for a read that moved, each task that defined the moved cell, and last the edited
/// file's outline, entered because its key moved. CONTROL: `why` of the file alone is that last
/// step; an item the file does not have is said, not guessed.
#[test]
fn engine_why_walks_back_to_the_edit() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    two_crates(&io);
    write(&io, "b/src/lib.rs", "use a::*;\n\npub fn h() -> u8 {\n    f() + m::k()\n}\n");
    let _ = engine_from(&io, 2, false);
    write(&io, "a/src/lib.rs", "pub mod m;\n\npub fn f() -> u8 {\n    1\n}\n\npub fn f2() -> u8 {\n    3\n}\n");
    let (_, item) = engine_from(&io, 2, false);
    let said = why(&io, "b/src/lib.rs::h");
    let lines: std::vec::Vec<&str> = said.lines().collect();
    assert!(lines.len() >= 2, "a chain of steps: {said}\n{}", entries(&item));
    assert!(lines[0].starts_with("the body of `b/src/lib.rs::h`: entered in request") && lines[0].contains("a read moved"), "first, the body and its moved read: {said}");
    let last = lines[lines.len() - 1];
    assert!(last.starts_with("`a/src/lib.rs`'s outline: entered in request") && last.contains("its key moved"), "last, the edited file: {said}");
    let file = why(&io, "a/src/lib.rs");
    assert_eq!(file.lines().collect::<std::vec::Vec<_>>(), [last], "the file's own why is the last step");
    assert!(why(&io, "a/src/lib.rs::nothing").contains("has no item `nothing`"), "an item the file does not have is said");
}

/// One request's listed findings, its printed lines but the closing one (it holds the time and
/// the count entered), and its counts.
fn said(io: &Io, fresh: bool) -> (std::vec::Vec<(std::string::String, std::string::String)>, std::vec::Vec<std::string::String>, engine_check::Checked) {
    let before = stdout(io).len();
    let (ids, checked) = engine_from(io, 2, fresh);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    (ids, lines, checked)
}

/// Every request prints the WHOLE answer (engine_lines.rs): with nothing changed the certificate
/// prints the stored lines (a finding kept, no task entered); after a comment the names row that
/// holds the finding stands, and its finding is printed from the last request's lines, so the
/// request prints what a fresh request on the same text prints, line for line, and exits alike.
/// CONTROL: the finding is there to print (the first request lists the unresolved import).
#[test]
fn engine_check_prints_the_whole_answer_from_its_stored_lines() {
    let io = Io::simulated(SimWorld::new(12, Limits::USUAL));
    two_crates(&io);
    let (ids, first, checked) = said(&io, false);
    assert!(!ids.is_empty(), "the unresolved import is listed: {}", entries(&checked));
    let (again_ids, again, certificate) = said(&io, false);
    assert!(certificate.certified && certificate.entered == 0, "nothing changed with a finding kept: the certificate; {}", entries(&certificate));
    assert_eq!((again_ids, again), (ids.clone(), first.clone()), "the certificate prints the first request's lines");
    assert_eq!(certificate.status.0, checked.status.0, "the certificate exits as the first request");

    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n// a comment\n");
    let (comment_ids, comment, incremental) = said(&io, false);
    assert!(!incremental.certified && incremental.entered < incremental.tasks, "a comment enters a part: {}", entries(&incremental));
    assert_eq!(incremental.stages[blaze_engine::Stage::Names.index()].entered, 0, "no crate's names entered: {}", entries(&incremental));
    let (fresh_ids, fresh, fresh_checked) = said(&io, true);
    assert_eq!(comment_ids, fresh_ids, "a comment lists what a fresh request lists");
    assert_eq!(comment, fresh, "a comment prints what a fresh request prints, line for line");
    assert_eq!(incremental.status.0, fresh_checked.status.0, "and exits alike");
    assert_eq!(comment_ids, ids, "the finding of the row that stood is listed");
}

/// One crate of three files, eight fns calling each other across its modules (paths from the
/// crate root, a module path, a glob import).
fn eight_fns(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "mod geom;\nmod stats;\n\npub fn top() -> u32 {\n    geom::area(2, 3) + stats::mean(4, 6)\n}\n\npub fn helper() -> u32 {\n    stats::total(1, 2)\n}\n");
    write(io, "src/geom.rs", "use crate::stats::*;\n\npub fn area(w: u32, h: u32) -> u32 {\n    scale(w) * h\n}\n\nfn scale(x: u32) -> u32 {\n    x + crate::stats::total(0, 0)\n}\n\npub fn side() -> u32 {\n    total(1, 1)\n}\n");
    write(io, "src/stats.rs", "pub fn total(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mean(a: u32, b: u32) -> u32 {\n    total(a, b) / 2\n}\n\npub fn spread() -> u32 {\n    crate::geom::side() + 1\n}\n");
}

/// NAMES READ BY CELL (engine.md E4; lane NC): a body's names reads are the cells of its
/// crate's cell table its walks consulted (engine_bodies.rs `widen`), and the crate's `Names`
/// task defines every cell of that table (engine_names.rs `cells_of`), so an edit that moves the
/// crate's names enters only the bodies whose consulted cells moved. Each edit below enters its
/// file's outline (its key moved), the crate's names (its surface read moved), and exactly the
/// bodies named, each for its cause:
/// - a private fn added: that fn's body alone, new;
/// - a pub fn added that nobody calls: the same;
/// - a `use` added that shadows a glob's name in one module: the one body there that looks
///   that name up (its binding cell, absent before, moved), nobody else;
/// - a pub fn renamed: the renamed body (new) and each of its three callers (their lookup's
///   cell moved), which then fail; nobody else.
/// CONTROL: before the cells were defined, the private fn added entered every body of the crate
/// (plain3, tag g120452: "Bodies read moved Name { kind: CrateScope, .. }: 11").
#[test]
fn adding_a_private_fn_rechecks_one_body() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    eight_fns(&io);
    let (_, first) = engine_from(&io, 2, false);
    println!("the first request: {}, status {}, bodies {:?}", entries(&first), first.status.0, first.bodies);
    // (Its status is 3 with no note: "NOT checked: borrows" while borrows stay notes.)
    assert_eq!(first.bodies.1, 8, "eight bodies checked clean: {:?}", first.bodies);
    let bodies = |checked: &engine_check::Checked| -> std::vec::Vec<(&'static str, Option<blaze_drive::Kind>, u32)> {
        let mut groups: std::vec::Vec<_> = checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies).map(|(group, count)| (group.cause, group.definer.map(|name| name.kind), *count)).collect();
        groups.sort();
        groups
    };
    let stage = |checked: &engine_check::Checked, stage: blaze_engine::Stage| checked.stages[stage.index()].entered;
    let names = Some(blaze_drive::Kind::CrateScope);
    // Each edit: its file's outline (key moved), the crate's names (a read moved), then the
    // bodies: `by_names` of them entered because a names cell moved (definer `CrateScope`),
    // `new` of them new; `exact`, when given, is the whole Bodies list by cause.
    let edit = |what: &str, path: &str, text: &str, by_names: Option<u32>, new: u32, exact: Option<&[(&'static str, Option<blaze_drive::Kind>, u32)]>, certify: bool| -> engine_check::Checked {
        write(&io, path, text);
        let (_, checked) = engine_from(&io, 2, false);
        println!("{what}: {}", entries(&checked));
        assert_eq!(stage(&checked, blaze_engine::Stage::Outlines), 1, "{what}: its file's outline: {}", entries(&checked));
        assert!(stage(&checked, blaze_engine::Stage::Names) >= 1, "{what}: the crate's names: {}", entries(&checked));
        assert!(
            checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Names).all(|(group, _)| group.cause == "read moved"),
            "{what}: names entered for a read that moved alone: {}",
            entries(&checked)
        );
        let got = bodies(&checked);
        let count = |cause: &str, definer: Option<blaze_drive::Kind>| got.iter().filter(|(c, d, _)| *c == cause && (definer.is_none() || *d == definer)).map(|(_, _, n)| *n).sum::<u32>();
        if let Some(by_names) = by_names {
            assert_eq!(count("read moved", names), by_names, "{what}: bodies entered for a names cell: {}", entries(&checked));
        }
        assert_eq!(count("new", None), new, "{what}: new bodies: {}", entries(&checked));
        if let Some(exact) = exact {
            assert_eq!(got, exact, "{what}: the bodies entered, by cause: {}", entries(&checked));
        }
        if certify {
            let (_, again) = engine_from(&io, 2, false);
            assert!(again.certified && again.entered == 0, "nothing changed after {what}: the certificate; {}", entries(&again));
        }
        checked
    };
    // A private fn added: no body for a names cell; the new fn's body, new. What else is entered
    // is lane EW's: every body that read a head of the file is entered by the file's `HEADS`
    // cell (engine_tasks.rs, INTERIM until a Heads stage defines a cell per head).
    let geom = "use crate::stats::*;\n\npub fn area(w: u32, h: u32) -> u32 {\n    scale(w) * h\n}\n\nfn scale(x: u32) -> u32 {\n    x + crate::stats::total(0, 0)\n}\n\npub fn side() -> u32 {\n    total(1, 1)\n}\n";
    edit("a private fn added", "src/geom.rs", &format!("{geom}\nfn extra() -> u32 {{\n    7\n}}\n"), Some(0), 1, None, true);
    let stats = "pub fn total(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mean(a: u32, b: u32) -> u32 {\n    total(a, b) / 2\n}\n\npub fn spread() -> u32 {\n    crate::geom::side() + 1\n}\n";
    edit("a pub fn added nobody calls", "src/stats.rs", &format!("{stats}\npub fn unused() -> u32 {{\n    9\n}}\n"), Some(0), 1, None, true);
    // A `use` no body reads: the names move and no body is entered for a names cell. (Today
    // every body is still entered by `HEADS`, lane EW's interim, all ten coming out identical,
    // tag of 13:05; with per-head cells this edit's whole Bodies list is empty.)
    let lib = "mod geom;\nmod stats;\n\npub fn top() -> u32 {\n    geom::area(2, 3) + stats::mean(4, 6)\n}\n\npub fn helper() -> u32 {\n    stats::total(1, 2)\n}\n";
    edit("a `use` no body reads", "src/lib.rs", &format!("use stats::spread;\n{lib}"), Some(0), 0, None, true);
    // A `use` that shadows the glob's `total` in `geom`: `side`, which looks `total` up there
    // (its binding cell, absent before, moved), is entered; with `HEADS` moving too the cause
    // names whichever moved read comes first, so only "entered, not new" is asserted.
    let shadow = edit("a `use` added", "src/geom.rs", &format!("use crate::stats::mean as total;\n{geom}\nfn extra() -> u32 {{\n    7\n}}\n"), None, 0, None, true);
    assert!(bodies(&shadow).iter().any(|(cause, _, _)| *cause == "read moved"), "a `use` added: `side` entered: {}", entries(&shadow));
    // A pub fn renamed: the renamed body, new; its three callers (`mean`, `scale`, `helper`)
    // entered, by a names cell or by the file's `HEADS` (the first read that moved names the
    // cause), and they then fail.
    let renamed = stats.replace("pub fn total(", "pub fn sum(");
    let checked = edit("a pub fn renamed", "src/stats.rs", &format!("{renamed}\npub fn unused() -> u32 {{\n    9\n}}\n"), None, 1, None, false);
    let entered: u32 = bodies(&checked).iter().filter(|(cause, _, _)| *cause != "new").map(|(_, _, n)| *n).sum();
    assert!(entered >= 3, "a pub fn renamed: its three callers entered: {}", entries(&checked));
}
