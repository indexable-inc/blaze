//! A FILE THE NAMES FIND IS PLANNED (D161, `Did::Need` on the phase path): the file walk plans
//! no `mod` a macro writes (here from a computed name, `macro_rules! m { ($n:ident) => { mod
//! $n; } } m!(a);`; a `mod a;` echoed from a call's tokens: engine_macro_mods_tests.rs); the
//! crate's names read `src/a.rs`, outline it on their worker as the walk outlines a file
//! (engine_check.rs `outline_found`), and once every names task has answered the file is planned
//! through the one engine call (`Engine::plan_file`, engine_check.rs `serve_found`): its outline
//! filed, its place kept with the units that found it (`PLACE_FOUND_LIB`, `PLACE_FOUND_TEST`),
//! its heads, bodies and lints planned. The next request takes it from that place as the walk's
//! own file, on either driver. Every crate whose files the walk outlines is a member (a path
//! dependency too, blaze-world crate_list.rs `member`); a fetched crate's files are read from
//! its pack by its names, never outlined, so there is nothing of it to find. A child of
//! main.rs's tests.
//!
//! The rule (the Reference, "Macros By Example"): `mod $n;` transcribed with `$n = a` is
//! `mod a;` in the module of the call; "Modules": its file is `a.rs` beside a `mod.rs`-less
//! module's directory; "Conditional compilation": a module under a false `cfg` is no module.

use super::engine_check_tests::{boxed, engine_from, entries, error_lines, opts};
use super::engine_macro_mods_tests::{BAD, GOOD, checked, good_and_bad, refused};
use super::*;

use blaze_drive::Name;
use blaze_engine::Stage;

/// The macro that writes `mod NAME;` from its input: no token of a call is `mod`.
const COMPUTED: &str = "macro_rules! m {\n    ($n:ident) => { mod $n; };\n}\n";

/// The crate root that declares `a` through the macro, and its literal twin.
fn found_lib() -> std::string::String {
    std::format!("{COMPUTED}m!(a);\npub fn top() -> u8 {{ a::f() }}\n")
}
const LITERAL_LIB: &str = "mod a;\npub fn top() -> u8 { a::f() }\n";

/// What the stored answer says of one file: its plan's digest (the root's `files`), its place,
/// and its outline's row with the request's serial masked (the rest is from text).
type FileSaid = (Option<Option<blaze_ids::ContentId>>, Option<blaze_engine::pages::FilePlace>, Option<blaze_engine::last::LastRow>);

pub(super) fn stored_file(io: &Io, path: &str) -> FileSaid {
    use blaze_engine::last::LastAnswer as _;
    let file = Name::file(path.as_bytes());
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let alloc = opts.heap.shared();
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("an answer is stored").expect("its pointer reads");
        let directory = crate::engine_answer::open_root(cx, store, pointer.root, alloc).expect("the store reads").expect("its root reads");
        let snapshot = store.snapshot_in(alloc).expect("a snapshot of the store");
        let stored = crate::engine_answer::Stored::new(&directory, &snapshot, &pointer.headers, blaze_engine::pages::Check::Store, alloc);
        let rows = stored.answer();
        let row = rows.task(&file).and_then(|task| rows.row(task).expect("a row reads")).map(|row| blaze_engine::last::LastRow { serial: blaze_answer::Serial(0), ..row });
        (directory.files.get(&file).copied(), directory.places.get(&file).copied(), row)
    })
}

/// The place with the found flags masked: the one difference a found file's may have.
fn unflagged(said: FileSaid) -> FileSaid {
    let (plan, place, row) = said;
    (plan, place.map(|place| blaze_engine::pages::FilePlace { flags: place.flags & !crate::engine_check::PLACE_FOUND, ..place }), row)
}

/// Every file of the stored answer, by name: its plan's digest and its place with the found
/// flags masked (the one difference a found file's may have).
fn stored_plans(io: &Io) -> std::vec::Vec<(Name, Option<blaze_ids::ContentId>, Option<blaze_engine::pages::FilePlace>)> {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let alloc = opts.heap.shared();
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("an answer is stored").expect("its pointer reads");
        let directory = crate::engine_answer::open_root(cx, store, pointer.root, alloc).expect("the store reads").expect("its root reads");
        let mask = |place: blaze_engine::pages::FilePlace| blaze_engine::pages::FilePlace { flags: place.flags & !crate::engine_check::PLACE_FOUND, ..place };
        directory.files.iter().map(|(file, plan)| (*file, *plan, directory.places.get(file).copied().map(mask))).collect()
    })
}

/// A fresh check of `files` (`Cargo.toml` the one-crate manifest) in a world kept for the next
/// request.
pub(super) fn world(seed: u64, files: &[(&str, &str)]) -> Io {
    let mut all = std::vec::Vec::from([("Cargo.toml", MANIFEST)]);
    all.extend_from_slice(files);
    world_of(seed, &all)
}

/// A fresh check of exactly `files` in a world kept for the next request.
fn world_of(seed: u64, files: &[(&str, &str)]) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    for (path, text) in files {
        write(&io, path, text);
    }
    let _ = engine_from(&io, 2, true);
    io
}

/// THE CONTROL (rule 23): the serve planted away (engine_check.rs `without_found_serve`, the
/// cause removed), the names' read of `src/a.rs` stops the check as before this piece, named by
/// the file (`UnseenFile`).
#[test]
fn without_the_serve_a_found_file_is_the_unseen_read() {
    let lib = found_lib();
    let words = crate::engine_check::without_found_serve(|| refused(21, &[("src/lib.rs", lib.as_str()), ("src/a.rs", GOOD)]));
    println!("without the serve: {words}");
    assert!(words.contains("which its plan did not hand it") && words.contains("`src/a.rs`") && words.contains("without_found_serve"), "the refusal names the file and the plant:\n{words}");
}

/// A plain `mod b;` inside the found file: the coordinator outlines the found file's parts as
/// the walk does (`serve_found` step 2), so `src/a/b.rs` is planned and checked.
#[test]
fn a_module_inside_a_found_file_is_planned() {
    let lib = std::format!("{COMPUTED}m!(a);\npub fn top() -> u8 {{ a::b::f() }}\n");
    good_and_bad(22, &lib, "src/a/b.rs", &[("src/a.rs", "pub mod b;\n")]);
}

/// REPLAY: the next request with nothing changed takes the found file from its place, plans it
/// as the walk's (equal plan digests), and enters nothing.
#[test]
fn a_found_file_replays_with_nothing_entered() {
    let lib = found_lib();
    let io = world(23, &[("src/lib.rs", lib.as_str()), ("src/a.rs", GOOD)]);
    let (_, again) = engine_from(&io, 2, false);
    println!("replay: {}", entries(&again));
    assert_eq!(again.entered, 0, "nothing changed, nothing entered: {}", entries(&again));
}

/// An edit of the found file's body enters what the same edit of its literal twin enters: its
/// outline and its bodies, and no names task the literal twin does not.
#[test]
fn an_edit_of_a_found_file_enters_what_the_literal_twin_enters() {
    let entered = |lib: &str| {
        let io = world(24, &[("src/lib.rs", lib), ("src/a.rs", GOOD)]);
        write(&io, "src/a.rs", "pub fn f() -> u8 { 2 }\n");
        let (_, edited) = engine_from(&io, 2, false);
        println!("edit under {lib:?}: {}", entries(&edited));
        (edited.stages[Stage::Outlines.index()].entered, edited.stages[Stage::Names.index()].entered, edited.stages[Stage::Bodies.index()].entered, edited.entered)
    };
    let found = entered(&found_lib());
    let literal = entered(LITERAL_LIB);
    assert!(found.2 > 0, "the edited body is entered: {found:?}");
    assert_eq!(found, literal, "(outlines, names, bodies, all) entered: the found file's as the literal twin's");
}

/// THE FOUND FILE IS THE LITERAL ONE (condition 1): its plan's digest (every task's name, phase
/// and key), its outline's row and its place but for the found flags are byte-equal to those of
/// the same file under a literal `mod`; both units' flags are set. At the root and inside an
/// inline module (`mod outer { m!(inner); }`, `src/outer/inner.rs`).
#[test]
fn a_found_file_is_planned_as_its_literal_twin() {
    let cases = [
        (found_lib(), LITERAL_LIB.to_owned(), "src/a.rs"),
        (std::format!("{COMPUTED}mod outer {{\n    m!(inner);\n}}\n"), "mod outer {\n    mod inner;\n}\n".to_owned(), "src/outer/inner.rs"),
    ];
    for (found, literal, path) in cases.iter() {
        let path: &str = path;
        let found_io = world(25, &[("src/lib.rs", found.as_str()), (path, GOOD)]);
        let literal_io = world(25, &[("src/lib.rs", literal.as_str()), (path, GOOD)]);
        let (by_macro, by_mod) = (stored_file(&found_io, path), stored_file(&literal_io, path));
        println!("{path}: found {by_macro:?}\nliteral {by_mod:?}");
        assert!(by_mod.0.is_some() && by_mod.1.is_some() && by_mod.2.is_some(), "the literal twin plans {path}");
        let flags = by_macro.1.map(|place| place.flags & crate::engine_check::PLACE_FOUND);
        assert_eq!(flags, Some(crate::engine_check::PLACE_FOUND), "both units' names read {path}");
        assert_eq!(unflagged(by_macro), unflagged(by_mod), "{path}: plan, place (found flags masked) and outline row equal");
    }
}

/// A PATH DEPENDENCY'S FOUND FILE: `app` (the one listed member) uses `dep::a::f` of its path
/// dependency `dep`, whose `pub mod a;` a macro writes from a computed name. `dep` is a member
/// (blaze-manifest layout: a path dependency under the root follows its dependent), so its
/// names find `dep/src/a.rs` and it is served with `dep`'s root as its place's unit: every
/// file's plan digest and place (found flags masked) in the stored answer, the found file's
/// outline row, and what the check prints (good: clean; bad: the one E0308 in
/// `dep/src/a.rs`) are those of the literal `pub mod a;`, but for `dep/src/lib.rs`'s own
/// plan, whose text differs.
#[test]
fn a_path_dependency_s_found_file_is_planned_as_its_literal_twin() {
    const DEP_COMPUTED: &str = "macro_rules! m {\n    ($n:ident) => { pub mod $n; };\n}\nm!(a);\n";
    const DEP_LITERAL: &str = "pub mod a;\n";
    let path = "dep/src/a.rs";
    let lib_file = Name::file(b"dep/src/lib.rs");
    let said = |dep_lib: &str, body: &str| {
        let files = [
            ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n"),
            ("app/src/lib.rs", "pub fn top() -> u8 {\n    dep::a::f()\n}\n"),
            ("dep/Cargo.toml", "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            ("dep/src/lib.rs", dep_lib),
            (path, body),
        ];
        let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
        for (at, text) in files.iter() {
            write(&io, at, text);
        }
        let before = stdout(&io).len();
        let (_, checked) = engine_from(&io, 2, true);
        let printed = stdout(&io).split_off(before);
        let mut errors = error_lines(&printed);
        errors.sort();
        let failed = checked.status.0 == Status::FAILED.0;
        let mut plans = stored_plans(&io);
        plans.retain(|(file, ..)| *file != lib_file);
        (failed, errors, plans, unflagged(stored_file(&io, path)), stored_file(&io, path).1, printed)
    };
    for (body, wrong) in [(GOOD, false), (BAD, true)] {
        let found = said(DEP_COMPUTED, body);
        let literal = said(DEP_LITERAL, body);
        println!("path dependency (wrong {wrong}): found {:?}\nliteral {:?}\n{}", found.4, literal.4, found.5);
        assert!(!found.5.contains("which its plan did not hand it"), "the read of {path} is served:\n{}", found.5);
        assert_eq!(found.0, wrong, "the check fails exactly when {path} holds the bad body:\n{}", found.5);
        assert_eq!(found.1.len(), usize::from(wrong), "the bad body's one error and no other:\n{}", found.5);
        assert!(found.1.iter().all(|line| line.contains("dep/src/a.rs:")), "the error is placed in {path}:\n{}", found.5);
        let place = found.4.expect("the found file has a place");
        assert!(place.flags & crate::engine_check::PLACE_FOUND_LIB != 0 && place.unit == lib_file, "found by dep's lib unit, placed under dep's root: {place:?}");
        assert!(literal.3.2.is_some(), "the literal twin outlines {path}");
        assert_eq!((found.0, &found.1, &found.2, &found.3), (literal.0, &literal.1, &literal.2, &literal.3), "printed errors, every other file's plan and place, and {path}'s plan, place and row: the literal twin's");
    }
}

/// THE READY DRIVER ON A FOUND FILE: the found file in the stored answer, its body edited (no
/// signature moves, so no names task enters and `--driver=ready` admits the edit,
/// engine_ready_edit.rs `admit`): the ready driver plans it from its kept place as any file,
/// checks the bad body once, in `src/a.rs`, and leaves the stored file and the entered count of
/// the same edit of the literal twin. CONTROL (the edit is a ready one): the driver refuses any
/// request it cannot serve by name (`ReadyRefused`), which would fail this test, never run the
/// phase path in its place.
#[test]
fn a_found_file_s_body_edit_on_the_ready_driver_is_the_literal_twin_s() {
    let edited = |lib: &str| {
        let io = world(32, &[("src/lib.rs", lib), ("src/a.rs", GOOD)]);
        write(&io, "src/a.rs", BAD);
        let mut ready = opts(&io, 2, false);
        ready.driver = crate::engine_ready_edit::Driver::Ready;
        let before = stdout(&io).len();
        let (_, checked) = super::engine_check_tests::engine_opts(&io, ready);
        let printed = stdout(&io).split_off(before);
        println!("ready edit under {lib:?}: {}\n{printed}", entries(&checked));
        (checked.status.0 == Status::FAILED.0, error_lines(&printed), checked.entered, unflagged(stored_file(&io, "src/a.rs")))
    };
    let found = edited(&found_lib());
    let literal = edited(LITERAL_LIB);
    assert!(found.0 && found.1.len() == 1 && found.1[0].contains("src/a.rs:"), "the ready driver checks the found file's bad body, once: {found:?}");
    assert_eq!(found, literal, "(failed, errors, entered, the stored file): the literal twin's");
}

/// THE DIVERGENCE UNDER A FALSE CFG (accepted, asserted): `#[cfg(any())] mod outer { m!(inner); }`
/// is never outlined, where its literal twin is. The walk is cfg-blind: it outlines the file a
/// literal `mod` names under any cfg, and no unit's names hold it, so its one body is OWNERLESS:
/// not checked, and counted in the note (engine_check.rs `Checked::ownerless`, which the shared
/// helpers hold at 0, so the twin runs through [`ownerless_world`]); the names never expand a
/// macro call in an item a false cfg removes, so no names task reads `inner.rs` and nothing
/// finds it: no ownerless body. Both check clean with a bad body there.
#[test]
fn a_found_file_under_a_false_cfg_is_never_outlined() {
    let found = std::format!("{COMPUTED}#[cfg(any())]\nmod outer {{\n    m!(inner);\n}}\npub fn top() -> u8 {{ 1 }}\n");
    let literal = "#[cfg(any())]\nmod outer {\n    mod inner;\n}\npub fn top() -> u8 { 1 }\n";
    let path = "src/outer/inner.rs";
    let (failed, said) = checked(26, &[("src/lib.rs", found.as_str()), (path, BAD)]);
    assert!(!failed && error_lines(&said).is_empty(), "a module under a false cfg is no module:\n{said}");
    let found_io = world(26, &[("src/lib.rs", found.as_str()), (path, BAD)]);
    let (literal_io, literal_checked, literal_said) = ownerless_world(26, &[("src/lib.rs", literal), (path, BAD)]);
    let literal_failed = literal_checked.status.0 == Status::FAILED.0;
    assert!(!literal_failed && error_lines(&literal_said).is_empty(), "the literal twin under a false cfg checks clean:\n{literal_said}");
    let (ownerless, ownerless_paths) = &literal_checked.ownerless;
    assert!(*ownerless == 1 && ownerless_paths.iter().any(|one| one.as_str() == path), "the literal twin's one body is ownerless, in {path}: {ownerless} in {ownerless_paths:?}");
    let (by_macro, by_mod) = (stored_file(&found_io, path), stored_file(&literal_io, path));
    println!("cfg(any()): found {by_macro:?}\nliteral {by_mod:?}");
    assert_eq!(by_macro, (None, None, None), "no names read it: never outlined, no place, no row");
    assert!(by_mod.1.is_some() && by_mod.2.is_some(), "the cfg-blind walk outlines the literal twin's file");
}

/// A fresh check of `files` in a world kept for the next request, with what it printed and its
/// [`engine_check::Checked`] as it is: no helper's `ownerless == 0` asserted (the literal twin
/// of a file under a false cfg holds an ownerless body by design).
fn ownerless_world(seed: u64, files: &[(&str, &str)]) -> (Io, engine_check::Checked, std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    for (path, text) in files {
        write(&io, path, text);
    }
    let before = stdout(&io).len();
    let checked = {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        let (out, err) = (io.need_out().expect("out"), io.need_err().expect("err"));
        let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
        let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
        let fs = io.need_fs().expect("files");
        let host = io.need_host().expect("host");
        let mut rng = io.need_rng().expect("rng");
        let args = io.args();
        let net = io.net();
        match engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts(&io, 2, true), &mut shown, &mut phases, &out, &err) {
            Ok(checked) => checked,
            Err(error) => panic!("engine-check answers: {error:?}"),
        }
    };
    let said = stdout(&io).split_off(before);
    (io, checked, said)
}

/// `#[cfg(test)] mod outer { m!(inner); }`: only the test unit's names expand the call, so only
/// its flag is set (the lib unit reaches no row of the file) and the test unit checks its body.
#[test]
fn a_found_file_under_cfg_test_is_the_test_units_alone() {
    let lib = std::format!("{COMPUTED}#[cfg(test)]\nmod outer {{\n    m!(inner);\n}}\npub fn top() -> u8 {{ 1 }}\n");
    let path = "src/outer/inner.rs";
    let (failed, said) = checked(27, &[("src/lib.rs", lib.as_str()), (path, BAD)]);
    println!("cfg(test): failed {failed}\n{said}");
    let errors = error_lines(&said);
    assert!(failed && errors.len() == 1 && errors[0].contains("src/outer/inner.rs:"), "the test unit checks the bad body, once:\n{said}");
    let io = world(27, &[("src/lib.rs", lib.as_str()), (path, BAD)]);
    let flags = stored_file(&io, path).1.map(|place| place.flags & crate::engine_check::PLACE_FOUND);
    assert_eq!(flags, Some(crate::engine_check::PLACE_FOUND_TEST), "the test unit's flag alone");
}

/// THE DROP (condition 2): the macro stops writing `a`; the next request's names read no
/// `src/a.rs`, so the file is dropped (`Engine::drop_file`): no plan, place or row, and its bad
/// body prints nothing. CONTROL: before the edit the answer held all three.
#[test]
fn a_found_file_is_dropped_when_no_names_read_it() {
    let lib = found_lib();
    let io = world(28, &[("src/lib.rs", lib.as_str()), ("src/a.rs", GOOD)]);
    let before = stored_file(&io, "src/a.rs");
    assert!(before.0.is_some() && before.1.is_some() && before.2.is_some(), "control: the found file is planned: {before:?}");
    write(&io, "src/lib.rs", "pub fn top() -> u8 { 1 }\n");
    write(&io, "src/a.rs", BAD);
    let printed = stdout(&io).len();
    let (_, after) = engine_from(&io, 2, false);
    let said = stdout(&io).split_off(printed);
    println!("dropped: {}\n{said}", entries(&after));
    assert!(error_lines(&said).is_empty(), "a dropped file's body is not checked:\n{said}");
    assert_eq!(stored_file(&io, "src/a.rs"), (None, None, None), "no plan, place or row of the dropped file");
}

/// THE CAP (condition 3, rule 18): a crate whose names find more files than the found cap
/// ([`crate::engine_check::FOUND_CAP`], set to 3 here by `with_found_cap`) stops, typed, naming
/// the crate, the count and the cap. CONTROL: as many files as the cap are served and checked.
#[test]
fn found_files_past_the_cap_are_refused() {
    const CAP: usize = 3;
    let crate_of = |count: usize| {
        let mut lib = std::string::String::from(COMPUTED);
        for at in 0..count {
            lib.push_str(&std::format!("m!(a{at});\n"));
        }
        let paths: std::vec::Vec<std::string::String> = (0..count).map(|at| std::format!("src/a{at}.rs")).collect();
        (lib, paths)
    };
    let (lib, paths) = crate_of(CAP);
    let mut files: std::vec::Vec<(&str, &str)> = std::vec::Vec::from([("src/lib.rs", lib.as_str())]);
    files.extend(paths.iter().map(|path| (path.as_str(), GOOD)));
    let (failed, said) = crate::engine_check::with_found_cap(CAP, || checked(29, &files));
    assert!(!failed && error_lines(&said).is_empty(), "control: {CAP} found files under a cap of {CAP} are served:\n{said}");
    let (lib, paths) = crate_of(CAP + 1);
    let mut files: std::vec::Vec<(&str, &str)> = std::vec::Vec::from([("src/lib.rs", lib.as_str())]);
    files.extend(paths.iter().map(|path| (path.as_str(), GOOD)));
    let words = crate::engine_check::with_found_cap(CAP, || refused(29, &files));
    println!("past the cap: {words}");
    let counted = std::format!("read {} files", CAP + 1);
    let cap = std::format!("past the cap of {CAP} files");
    assert!(words.contains("crate `") && words.contains(&counted) && words.contains(&cap), "the cap's refusal names the crate, the count and the cap:\n{words}");
}

/// ONE ANSWER WHATEVER THE ORDER (rule 25): two found files declared in either order are planned
/// alike: each file's plan, place and outline row are equal, and so are the errors printed.
#[test]
fn two_found_files_in_either_order_are_one_answer() {
    let orders = [std::format!("{COMPUTED}m!(a);\nm!(b);\n"), std::format!("{COMPUTED}m!(b);\nm!(a);\n")];
    let mut saids = std::vec::Vec::new();
    for lib in orders.iter() {
        let (failed, printed) = checked(30, &[("src/lib.rs", lib.as_str()), ("src/a.rs", BAD), ("src/b.rs", BAD)]);
        let mut errors = error_lines(&printed);
        errors.sort();
        assert!(failed && errors.len() == 2, "each found file's bad body is checked:\n{printed}");
        let io = world(30, &[("src/lib.rs", lib.as_str()), ("src/a.rs", BAD), ("src/b.rs", BAD)]);
        saids.push((errors, stored_file(&io, "src/a.rs"), stored_file(&io, "src/b.rs")));
    }
    assert_eq!(saids[0], saids[1], "the two orders give one answer of the found files");
}
