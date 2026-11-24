//! The engine's check on one checkout (engine.md E6), stage by stage. A child of main.rs's
//! tests: its helpers are theirs.
//!
//! Until 2026-11-19 these tests compared what the engine printed with what the old machine
//! printed; the old machine is gone, so each test's oracle is now the finding's own code, place
//! and status, and the engine entering each planned task once.

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
pub(super) fn engine(io: &Io, workers: u32) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    engine_from(io, workers, true)
}

/// The command's options as the tests' `check` runs (main.rs tests `check_opts`), on the checkout `ws`.
/// Its request frame is leaked: a cap whose blocks are taken as used (engine_check.rs
/// `REQUEST_FRAME`), one a call, as `engine_check::serve` makes one a request.
pub(super) fn opts(io: &Io, workers: u32, fresh: bool) -> engine_check::Opts<'static> {
    let frame: &'static bstd::mem::Frame = std::boxed::Box::leak(std::boxed::Box::new(bstd::mem::Frame::with_reserve(engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("the request's frame")));
    let producers = blaze_ids::producer::Producers::for_tests(b"test");
    engine_check::Opts {
        tools: engine_store::tools(&home::tools(&producers, lints::Lints::NONE.rules)),
        workers: core::num::NonZeroU32::new(workers).expect("a worker"),
        toolchain: shelf(io),
        heap: blaze_own::testing::heap(),
        place: engine_check::Place {
            checkout: "ws",
            served: crate::engine_script_reads::Served::Simulated,
            cert_file: None,
            cert_dir: None,
            rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
            home: Some("/h"),
            max_store_bytes: None,
            cargo_home: None,
            start: Some("/"),
            sdk: crate::sdk::NONE_IN_TESTS,
            exe_dir: Err(fetch::ExeDirWhy::NotRead),
            lan_peer: crate::fetch::LanPeerConfig::DISABLED,
        },
        fresh,
        heads_from: crate::engine_seeds::HeadsFrom::Frozen,
        head_units: crate::engine_heads::HeadUnits::Each,
        head_jobs: crate::engine_heads::HeadJobs::RULE,
        head_sources: crate::engine_heads_once::Sources::Job,
        frame,
        scope_reads: crate::engine_scope_reads::ScopeReadsAs::DEFAULT,
        unreadable: None,
        body_locals: blaze_ladder::front2::tables::seeds::BodyLocals::Lowered,
        status: crate::engine_status::Asking::NONE,
        origin: None,
        names_oracle: crate::engine_names::NamesOracle::Off,
        driver: crate::engine_ready_edit::Driver::Phases,
        ready_plant: crate::engine_ready_edit::CommitPlant::None,
    }
}

/// `blaze why <target>` on the checkout `ws`: what it printed.
pub(super) fn why(io: &Io, target: &str) -> std::string::String {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let before = stdout(io).len();
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    if let Err(error) = crate::engine_why::explain(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), target, &mut phases, &out, &err) {
        panic!("why {target} answers: {error:?}");
    }
    stdout(io).split_off(before)
}

/// [`engine`], starting from the stored answer unless `fresh`.
pub(super) fn engine_from(io: &Io, workers: u32, fresh: bool) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    engine_opts(io, opts(io, workers, fresh))
}

/// [`engine_from`] with the command's options as given (`opts`, one field changed by a test).
pub(super) fn engine_opts(io: &Io, opts: engine_check::Opts<'static>) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    engine_view(&cx, io, opts, View::Person)
}

/// `make()` on the heap, made in this call's own frame: a large value (a [`Phases`], 24 meter
/// arrays) is a temporary here, gone when it returns, never a slot in the caller's frame for
/// the whole check under it (the 2 MiB test stack, wave-1119 t29).
#[inline(never)]
/// `make`'s value boxed, made in this call's frame: `Box::new(Phases::new(..))` written in a test
/// keeps the 143 KiB value as a temporary in the TEST's frame for its whole call (debug builds
/// reserve every temporary at entry), under the check it then runs (wave-1119 t59 frames:
/// `standing_control` 166 KiB, `moduled_on` 164 KiB).
#[inline(never)]
pub(super) fn boxed<T>(make: impl FnOnce() -> T) -> std::boxed::Box<T> {
    std::boxed::Box::new(make())
}

pub(super) fn engine_view(cx: &bstd::Cx, io: &Io, opts: engine_check::Opts<'static>, view: View) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let fresh = opts.fresh;
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, view, "/ws", frame);
    let err = io.need_err().expect("err");
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    let checked = match engine_check::check(cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts, &mut shown, &mut phases, &out, &err) {
        Ok(checked) => checked,
        Err(error) => panic!("engine-check (fresh {fresh}) answers: {error:?}"),
    };
    // Every body's file is a member's: none "in files no crate holds" (engine_bodies.rs `reached`).
    assert_eq!(checked.ownerless.0, 0, "bodies in files no member crate holds (fresh {fresh}): first files {:?}", checked.ownerless.1);
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
    let unresolved = |ids: &[(std::string::String, std::string::String)]| -> std::vec::Vec<(std::string::String, std::string::String)> {
        let mut found: std::vec::Vec<_> = ids.iter().filter(|(code, _)| code == "unresolved").cloned().collect();
        found.sort();
        found
    };
    let mut answers = std::vec::Vec::new();
    for workers in [1, 4] {
        let (ids, checked) = engine(&io, workers);
        // The one import `a` does not declare (`use a::g;`), and nothing for the glob: the old
        // checker was this test's oracle until it went (2026-11-19); the rule is the oracle now.
        assert_eq!(unresolved(&ids).len(), 1, "engine-check with {workers} worker(s) lists the one unresolved import; it listed {ids:?}");        entered_once(&checked, workers);
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
        // A bound the fixture's own world proves: a trait of this crate and its impl for `u8`.
        // (`T: Copy` was an unresolved trait in a world with no std, which is E0405; and the lang
        // shelf's core declares `marker::Copy` with no impl for `u8`, so `pick(1u8)` is then an
        // unmet bound, as round 5 showed: "error[types]: `u8: Copy`".)
        Typed::Generic => "pub trait Mark {}\n\nimpl Mark for u8 {}\n\npub fn pick<T: Mark>(x: T) -> T {\n    x\n}\n",
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
        let types_not_checked = |said: &str| said.contains("NOT checked: types");
        let mut answers = std::vec::Vec::new();
        for workers in [1, 4] {
            let before = stdout(&io).len();
            // Generic needs a world where `Copy` exists: the lang shelf (main.rs tests `lang_shelf`).
            let (ids, checked) = if typed == Typed::Generic {
                let mut o = opts(&io, workers, true);
                o.toolchain = lang_shelf(&io);
                engine_opts(&io, o)
            } else {
                engine(&io, workers)
            };
            let said = stdout(&io).split_off(before);
            entered_once(&checked, workers);
            let bodies: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Bodies).map(|(_, planned, entered)| (*planned, *entered)).collect();
            // A task a body's row, each entered once: `h` and `one` or `pick`, and `wrong` when
            // written, each under its library and its library's test build (D191: every unit that
            // holds a body checks it, a row each, so 2 rows a body).
            let want = 2 * if typed == Typed::Wrong { 3 } else { 2 };
            assert_eq!(bodies, [(want, want)], "Bodies planned and entered with {workers} worker(s) on {typed:?}: two rows a body (D191: the library's and its test build's)");
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
            // The status by the rule (the old checker was the oracle until 2026-11-19): a type
            // error fails the command (1); otherwise it is incomplete while borrows are notes (3)
            // or clean (0), never 4 (no item left unchecked).
            assert_eq!(status == Status::FAILED.0, typed == Typed::Wrong, "engine-check's status with {workers} worker(s) on {typed:?}:\n{said}");
            assert_ne!(status, crate::UNCHECKED_ITEMS.0, "no item left unchecked with {workers} worker(s) on {typed:?}:\n{said}");
            match typed {
                Typed::Plain => {
                    assert!(notes.is_empty(), "the plain pair's bodies have no note with {workers} worker(s): {notes:?}");
                    assert_eq!(*clean, 2, "`one` and `h` checked clean with {workers} worker(s)");
                    assert!(!types_not_checked(&said), "types checked whole on the plain pair:\n{said}");
                }
                Typed::Generic => assert_eq!(notes.len() + *clean as usize, 2, "each body is clean or a note: {notes:?}"),
                Typed::Wrong => {
                    // The wrong body's type error is printed as `check` prints it, not a note.
                    assert!(notes.is_empty(), "the wrong body's finding is no note with {workers} worker(s): {notes:?}");
                    assert_eq!(error_lines(&said).len(), 1, "one type error printed:\n{said}");
                    assert!(!types_not_checked(&said), "types checked whole on the wrong pair:\n{said}");
                    assert_eq!(*clean, 2, "`one` and `h` checked clean with {workers} worker(s)");
                }
            }
            answers.push((ids, status, notes.clone(), checked.phases.iter().copied().collect::<std::vec::Vec<_>>()));
        }
        assert_eq!(answers[0], answers[1], "1 and 4 workers give the same answer on {typed:?}");
    }
}

/// EVERY BODY HAS AN OWNER (tempo, g124650: 13,539 of 16,042 bodies "in files no crate
/// holds"): three members, `x` with module files three `mod`s deep, a `tests/` file and a
/// `src/bin/x.rs`; `y` a library over `x`; `z` a binary with a module. Every body planned is
/// a member's (checked or refused by name), none ownerless (asserted in `engine_from`), and
/// each nested file's body is checked. CONTROL: before `reached`, a member whose records did
/// not open left its files' bodies ownerless with no word why.
#[test]
fn engine_check_finds_every_body_s_crate() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"crates/x\", \"crates/y\", \"bin/z\"]\nresolver = \"2\"\n");
    write(&io, "crates/x/Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "crates/x/src/lib.rs", "pub mod a;\n\npub fn top() -> u8 {\n    a::mid()\n}\n");
    write(&io, "crates/x/src/a.rs", "pub mod b;\n\npub fn mid() -> u8 {\n    b::low()\n}\n");
    write(&io, "crates/x/src/a/b.rs", "pub mod c;\n\npub fn low() -> u8 {\n    c::deep()\n}\n");
    write(&io, "crates/x/src/a/b/c.rs", "pub fn deep() -> u8 {\n    3\n}\n");
    write(&io, "crates/x/src/bin/x.rs", "fn main() {\n    let _ = x::top();\n}\n");
    write(&io, "crates/x/tests/t.rs", "fn uses() -> u8 {\n    x::top()\n}\n");
    write(&io, "crates/y/Cargo.toml", "[package]\nname = \"y\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nx = { path = \"../x\" }\n");
    write(&io, "crates/y/src/lib.rs", "pub fn twice() -> u8 {\n    x::top() + x::a::b::c::deep()\n}\n");
    write(&io, "bin/z/Cargo.toml", "[package]\nname = \"z\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "bin/z/src/main.rs", "mod util;\n\nfn main() {\n    let _ = util::one();\n}\n");
    write(&io, "bin/z/src/util.rs", "pub fn one() -> u8 {\n    1\n}\n");
    let (_, checked) = engine(&io, 2);
    let (notes, clean) = &checked.bodies;
    println!("engine-check owners: {clean} clean, notes {notes:?}, ownerless {:?}", checked.ownerless);
    // `top`, `mid`, `low`, `deep`, `twice`, `one` and z's `main` at least, each checked clean.
    assert!(*clean >= 7, "the members' bodies three `mod`s deep and in a binary are checked: {clean} clean, notes {notes:?}");
    for note in notes.iter() {
        assert!(!note.contains("deep") && !note.contains("`low`") && !note.contains("`one`"), "a nested module's body is checked: {notes:?}");
    }
}

/// The stack a whole check is stated to fit: a cold check and an edit of one body, one worker
/// (the bodies run inline on the caller's stack, `bstd::task::Scope::may_overlap`), measured on
/// a larger thread (`MEASURED_STACK`). Half the 2 MiB test stack the archive test overflowed at 2,109,888
/// bytes in 109 frames (wave-1119 t29, `checking` 523 KB and its drive closure 172 KB): the
/// command's stages are their own frames now (engine_check.rs `drive`, `lines_of`,
/// `print_answer`), so the depth is the stages', never their sum.
#[cfg(target_os = "macos")]
const CHECK_STACK: usize = 1 << 20;

/// The stack the guard's thread is given: far above [`CHECK_STACK`], so a check past its bound
/// is measured and refused by the assert, never an overflow that aborts the test binary.
#[cfg(target_os = "macos")]
const MEASURED_STACK: usize = 64 << 20;

/// What one check left on the guard's thread: the thread's stack high water after it, and how
/// deep each of the command's stages began (engine_check.rs `stack_probe`), deepest first.
#[cfg(target_os = "macos")]
struct StackUse {
    high_water: usize,
    stages: std::vec::Vec<(&'static str, usize)>,
}

#[cfg(target_os = "macos")]
impl StackUse {
    /// After a check on this thread: its high water so far and the stages' entry depths.
    fn now() -> StackUse {
        let high_water = blaze_sys::posix::thread::stack_high_water().expect("this thread's stack");
        let top = blaze_sys::posix::thread::this_thread_stack().expect("this thread's stack").high;
        let mut stages: std::vec::Vec<(&'static str, usize)> = crate::engine_check::stack_probe::take().into_iter().map(|(stage, sp)| (stage, top.saturating_sub(sp))).collect();
        stages.sort_unstable_by_key(|one| core::cmp::Reverse(one.1));
        StackUse { high_water, stages }
    }

    fn said(&self) -> std::string::String {
        let stages: std::vec::Vec<std::string::String> = self.stages.iter().map(|(stage, depth)| format!("{stage} at {} KiB", depth >> 10)).collect();
        format!("{} KiB (stages began: {})", self.high_water >> 10, stages.join(", "))
    }

    /// The oracle: the first bound this check broke, if any. The bodies must begin within
    /// [`BODIES_ENTRY`] (what the command's own frames above them may take: wave-1119 t39 measured
    /// 783 KiB, `checking` one 560 KiB frame), and the whole check within [`CHECK_STACK`].
    fn past_bound(&self) -> Option<PastBound> {
        let bodies = self.stages.iter().find(|one| one.0 == "bodies_phase").map(|one| one.1);
        match bodies {
            None => Some(PastBound::NoBodies),
            Some(depth) if depth > BODIES_ENTRY => Some(PastBound::BodiesBegan(depth)),
            Some(_) if self.high_water > CHECK_STACK => Some(PastBound::Whole(self.high_water)),
            Some(_) => None,
        }
    }
}

/// How deep the bodies may begin: the command's frames above them (`check`, `checking`, the
/// drive, the stages' own) together. The tree of wave-1119 t39 began them at 783 KiB.
#[cfg(target_os = "macos")]
const BODIES_ENTRY: usize = 512 << 10;

/// Which bound a check broke ([`StackUse::past_bound`]), with the depth it was broken at.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum PastBound {
    /// No body was checked: the bound is about nothing.
    NoBodies,
    /// The bodies began deeper than [`BODIES_ENTRY`].
    BodiesBegan(usize),
    /// The whole check went deeper than [`CHECK_STACK`].
    Whole(usize),
}

/// The planted control's large local: a frame of [`PAD`] bytes held across `check`, as a large
/// value kept on the stack above the bodies would be.
#[cfg(target_os = "macos")]
const PAD: usize = 600 << 10;

#[cfg(target_os = "macos")]
#[inline(never)]
fn under_a_large_local<R>(check: impl FnOnce() -> R) -> R {
    let pad = [7u8; PAD];
    core::hint::black_box(&pad);
    let ran = check();
    core::hint::black_box(&pad);
    ran
}

/// A WHOLE CHECK FITS ITS STATED STACK ([`CHECK_STACK`]), MEASURED: the smallest checkout checked
/// cold, then one body edited and checked from the stored answer, one worker (the bodies inline
/// on this stack), on a thread of [`MEASURED_STACK`]; after each, the thread's high water
/// (blaze-sys `stack_high_water`: its fresh stack's lowest written word) and the depth each
/// stage began at. The mark only rises, so the edit's is the larger of the two checks'. Printed
/// on every run, asserted under the bound naming the stages. CONTROL: the tree before the stage
/// split (`checking` one 523 KB frame) needed more than 2 MiB for the archive test's check.
/// macOS: Linux has no call for a thread's stack range in blaze-sys yet.
#[cfg(target_os = "macos")]
#[test]
fn a_whole_check_fits_its_stated_stack() {
    let ran = std::thread::Builder::new()
        .name("a check on a measured stack".into())
        .stack_size(MEASURED_STACK)
        .spawn(|| {
            let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
            write(&io, "Cargo.toml", MANIFEST);
            write(&io, "src/lib.rs", "pub fn one() -> u8 {\n    1\n}\n\npub fn two() -> u8 {\n    one() + 1\n}\n");
            let _ = crate::engine_check::stack_probe::take();
            let (_, cold) = engine_from(&io, 1, true);
            let cold_use = StackUse::now();
            write(&io, "src/lib.rs", "pub fn one() -> u8 {\n    2\n}\n\npub fn two() -> u8 {\n    one() + 1\n}\n");
            let (_, edited) = engine_from(&io, 1, false);
            let edit_use = StackUse::now();
            // PLANTED: the same cold check under a large local on its path: the oracle must
            // name the bodies' entry as past its bound.
            let (_, planted) = under_a_large_local(|| engine_from(&io, 1, true));
            let planted_use = StackUse::now();
            (cold.tasks, edited.entered, planted.tasks, cold_use, edit_use, planted_use)
        })
        .expect("a thread of the measured stack")
        .join();
    let (tasks, entered, planted_tasks, cold, edit, planted) = ran.expect("the check on the measured stack returned");
    println!("STACK: a cold check used {}; with the edit after it {}; under the planted {} KiB local {}; bounds: bodies began within {} KiB, the whole within {} KiB", cold.said(), edit.said(), PAD >> 10, planted.said(), BODIES_ENTRY >> 10, CHECK_STACK >> 10);
    assert!(tasks > 0 && entered > 0 && planted_tasks > 0, "every check ran tasks: cold {tasks} planned, edited {entered} entered, planted {planted_tasks} planned");
    assert!(matches!(planted.past_bound(), Some(PastBound::BodiesBegan(_))), "CONTROL: a {} KiB local above the bodies is caught: {}", PAD >> 10, planted.said());
    assert_eq!(cold.past_bound(), None, "a cold check within its bounds: {}", cold.said());
    assert_eq!(edit.past_bound(), None, "a check with an edit within its bounds: {}", edit.said());
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
        let (_, first_lines, first) = said(&io, false);
        assert!(first.bodies.0.is_empty() && error_lines(&first_lines.join("\n")).is_empty(), "the pair checks clean first: {:?}\n{first_lines:?}", first.bodies.0);
        write(&io, "a/src/lib.rs", &format!("{edited}{caller}"));
        let (now_ids, now_lines, now) = said(&io, false);
        let (fresh_ids, fresh_lines, fresh) = said(&io, true);
        // THE REAL RUN (g125544): both callers' errors printed, counted, and the status failed.
        let fresh_errors = error_lines(&fresh_lines.join("\n"));
        assert_eq!(fresh_errors.len(), 2, "a fresh run prints `two`'s and `h`'s errors after {edited:?}:\n{}", fresh_lines.join("\n"));
        assert_eq!(fresh.status.0, Status::FAILED.0, "two type errors fail the command after {edited:?}");
        assert!(closing(&io).contains("2 errors"), "the closing line counts both after {edited:?}: {}", closing(&io));
        assert_eq!((now_ids, error_lines(&now_lines.join("\n"))), (fresh_ids, fresh_errors), "the incremental request prints a fresh run's errors after {edited:?}\n{}", entries(&now));
        assert_eq!(now.bodies.0, fresh.bodies.0, "the incremental request's body notes are a fresh run's after {edited:?}\n{}", entries(&now));
        assert_eq!(now.status.0, fresh.status.0, "the incremental status is a fresh run's after {edited:?}");
        assert_eq!(now.stages[blaze_engine::Stage::Bodies.index()].entered, 6, "`one`, `two` and `h` entered after {edited:?}, each under its library and its test build (D191: a row each unit that holds the body)\n{}", entries(&now));
    }
}

/// The head reads of every body of the stored answer (engine_heads.rs `Owners::place`): each must
/// name a cell its head task defines, at the digest it defines, or it is a read no edit moves
/// (rule 21), returned in the list. The count: head reads judged good.
pub(super) fn head_reads_of_bodies(io: &Io) -> (std::vec::Vec<std::string::String>, u32) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let judged = crate::engine_why::with_kept(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), &mut phases, &err, |kept| {
        let rows = &kept.answer.rows;
        let (mut bad, mut good) = (std::vec::Vec::new(), 0u32);
        for (name, row) in rows.iter().filter(|(name, _)| name.kind == blaze_drive::Kind::BodyCheck) {
            for (id, digest) in row.reads.iter().filter(|(id, _)| id.of.kind == blaze_drive::Kind::Signature) {
                match rows.get(&id.of).and_then(|owner| owner.defines.iter().find(|(c, _)| *c == id.cell).map(|(_, d)| *d)) {
                    Some(d) if d == *digest => good += 1,
                    Some(d) => bad.push(format!("{name:?} reads {id:?} at {digest:?}, its head task defines {d:?}")),
                    None => bad.push(format!("{name:?} reads {id:?} at {digest:?}: no head task defines it")),
                }
            }
        }
        (bad, good)
    });
    match judged {
        Ok(Some(judged)) => judged,
        Ok(None) => panic!("no stored answer to judge the head reads of"),
        Err(error) => panic!("the stored answer: {error:?}"),
    }
}

/// A HEAD READ ACROSS CRATES IS ITS HEAD TASK'S CELL (lane EY): `two` and `one` in `a`, `b::h`
/// calling `a::one`: each of the five head reads (each body its own head, `two` and `h` also
/// `one`'s) is a cell `a`'s or `b`'s head task defines. CONTROL: with the cell named by the
/// item's checker key, `h`'s read of `one` named a cell no task defined (`b`'s unit keys `a`'s
/// items apart from `a`'s own scope), so `one`'s new signature did not enter `h`.
#[test]
fn every_head_read_of_a_body_is_a_head_task_cell() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    typed_crates(&io, Typed::Plain);
    write(&io, "a/src/lib.rs", "pub fn one() -> u8 {\n    1\n}\n\npub fn two() -> u8 {\n    one() + 1\n}\n");
    let _ = engine_from(&io, 2, false);
    let (bad, good) = head_reads_of_bodies(&io);
    assert!(bad.is_empty(), "head reads no head task answers:\n{}", bad.join("\n"));
    assert_eq!(good, 10, "the five head reads of the three bodies, each read by the body's two rows (D191: the library's and its test build's)");
}

/// A HEAD TASK PER ITEM (lane EY): a body is entered only when a head it read moved. One crate:
/// `P`, read by `make` and `get`; `area`, called by `top` and `helper`; `other`, called by
/// `top`. The six bodies are `make`, `get`, `area`, `other`, `top` and `helper`. `make`
/// constructs `P`, while `get` takes `&P` and reads `p.x`; both depend on `P`'s field list.
/// The other four bodies do not mention `P`. Each edit enters, exactly:
/// - a fn's parameter type changed: that fn's body (its key moved) and its two callers (their
///   read of its head moved); one head task: `area`'s, whose heads moved (see the edits below);
/// - a struct gains a field: the two bodies that read its head; `P`'s head task, moving, and its
///   readers', identical;
/// - a body edit: that body by its key, and the floor of the head tasks: one, its item's (keyed
///   by the item's tokens, engine_heads.rs `plan_items`), which makes the heads it made
///   (identical), so nothing reads a moved head;
/// - a comment inside a body: nothing (lane K1: a comment is no token, so neither the body's key
///   nor its item's head task's moves; its findings are token places, printed where they are now).
/// (A comment between items enters no head task: engine_check_starts_from_its_stored_answer.)
#[test]
fn a_moved_head_enters_exactly_its_readers() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    let lib = |p: &str, area: &str, other: &str| {
        format!(
            "pub struct P {{\n{p}}}\n\npub fn make() -> P {{\n    P {{ x: 1 }}\n}}\n\npub fn get(p: &P) -> u32 {{\n    p.x\n}}\n\n{area}\n\npub fn other() -> u32 {{\n{other}}}\n\npub fn top() -> u32 {{\n    area(2, 3) + other()\n}}\n\npub fn helper() -> u32 {{\n    area(1, 1)\n}}\n"
        )
    };
    let (p, area, other) = ("    pub x: u32,\n", "pub fn area(_w: u32, h: u32) -> u32 {\n    h\n}", "    3\n");
    write(&io, "src/lib.rs", &lib(p, area, other));
    let (_, first) = engine_from(&io, 2, false);
    println!("the first request: {}", entries(&first));
    // D191: every unit that holds a body checks it, a row each: the library's and its test build's.
    assert_eq!(first.stages[blaze_engine::Stage::Bodies.index()].entered, 12, "six bodies, two rows each (D191): {}", entries(&first));
    let (bad, _) = head_reads_of_bodies(&io);
    assert!(bad.is_empty(), "head reads no head task answers:\n{}", bad.join("\n"));
    let bodies = |checked: &engine_check::Checked| -> std::vec::Vec<(&'static str, Option<blaze_drive::Kind>, u32)> {
        let mut groups: std::vec::Vec<_> = checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies).map(|(group, count)| (group.cause, group.definer.map(|name| name.kind), *count)).collect();
        groups.sort();
        groups
    };
    let heads = |checked: &engine_check::Checked| {
        let s = &checked.stages[blaze_engine::Stage::Heads.index()];
        (s.entered, s.identical)
    };
    let sig = Some(blaze_drive::Kind::Signature);
    // `want_heads` `None`: the caller asserts the head tasks from the answer handed back.
    let edit = |what: &str, text: std::string::String, want_bodies: &[(&'static str, Option<blaze_drive::Kind>, u32)], want_heads: Option<(u32, u32)>| {
        write(&io, "src/lib.rs", &text);
        let (_, checked) = engine_from(&io, 2, false);
        println!("{what}: {}", entries(&checked));
        assert_eq!(bodies(&checked), want_bodies, "{what}: the bodies entered, by cause: {}", entries(&checked));
        if let Some(want_heads) = want_heads {
            assert_eq!(heads(&checked), want_heads, "{what}: the head tasks (entered, identical): {}", entries(&checked));
        }
        checked
    };
    let area_u64 = "pub fn area(_w: u64, h: u32) -> u32 {\n    h\n}";
    // The head tasks: the edited item's alone. It was (3, 2) before lane BH1, when every head's
    // key held its file's surface and a head task read another head of the file as the file's
    // `HEADS` cell, which any signature edit of the file moves (`make`'s and `get`'s read `P`,
    // entered and identical); a head task now reads a head of its own file by that head's
    // `Signature` cell (engine_heads.rs `heads_of`), and `area`'s is no head they read.
    // Body counts are rows (D191): each body's library row and its test build's row.
    edit("a parameter's type changed", lib(p, area_u64, other), &[("key moved", None, 2), ("read moved", sig, 4)], Some((1, 0)));
    let p2 = "    pub x: u32,\n    pub y: u32,\n";
    // `P`'s ADT head moves (its fields are its head), and `make`'s and `get`'s reads of that
    // head's `Signature` cell enter them again, both with identical body results. The old
    // `CrateScope` expectation came from a coarse whole-items read whose file text length and
    // spans moved for an unrelated edit. The names task now defines its cell table, so that
    // unrelated movement does not enter `area`, `other`, `top` or `helper`.
    let field = edit("a struct gains a field", lib(p2, area_u64, other), &[("read moved", sig, 4)], None);
    assert_eq!(field.stages[blaze_engine::Stage::Bodies.index()].entered, 4, "only the two bodies that use `P` are re-checked, two rows each (D191): {}", entries(&field));
    assert_eq!(bodies(&field), &[("read moved", sig, 4)], "only `make` and `get` re-check on `P`'s ADT head, two rows each (D191): {}", entries(&field));
    assert!(!field.because.iter().any(|(group, _)| group.stage == blaze_engine::Stage::Bodies && group.definer.is_some_and(|name| name.kind == blaze_drive::Kind::CrateScope)), "no body re-check is caused by an unrelated `CrateScope` cell: {}", entries(&field));
    let (entered, identical) = heads(&field);
    assert!((3..=7).contains(&entered) && identical + 1 == entered, "a struct gains a field: the head tasks (entered, identical) are `P`'s, moved, and readers that come out identical: {}", entries(&field));
    edit("a body edit", lib(p2, area_u64, "    4\n"), &[("key moved", None, 2)], Some((0, 0)));
    edit("a comment inside a body", lib(p2, area_u64, "    // four\n    4\n"), &[], Some((0, 0)));
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

/// The profile the last request wrote (`.blaze/v2/profile.txt` in the checkout).
pub(super) fn profile(io: &Io) -> std::string::String {
    profile_in(io, "ws")
}

/// The profile the last request in the checkout at `dir` wrote (its own `.blaze/v2/profile.txt`:
/// a second worktree's request writes in its checkout, never in `ws`).
pub(super) fn profile_in(io: &Io, dir: &str) -> std::string::String {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new(dir).expect("a path")).expect("open");
    let mut bytes = bstd::mem::Bytes::new_in(cx.frame(), "the profile");
    root.read_all(&cx, &Path::new(print::PROFILE).expect("a path"), &mut bytes).expect("the profile is there");
    std::string::String::from_utf8_lossy(bytes.as_slice()).into_owned()
}

/// The profile's `phases` and `files` lines.
pub(super) fn profile_lines(io: &Io) -> std::string::String {
    profile(io).lines().filter(|line| line.contains("phases") || line.starts_with("files:") || line.starts_with("stored answer:")).collect::<std::vec::Vec<_>>().join("\n")
}

/// ONLY CHANGED FILES ARE READ (engine.md E6, the incremental half): from the stored answer of
/// the plain pair with a module, a comment in one file outlines that file alone and reads no
/// other: the two unchanged files' outlines are read back from the store (their records hold
/// their parts, paths and findings), and the answer is a fresh run's. CONTROL: the first
/// request outlines all three; before the outlines were put with their listing, every request
/// outlined every file it reached.
#[test]
fn engine_check_outlines_only_the_changed_file() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    typed_crates(&io, Typed::Plain);
    write(&io, "a/src/lib.rs", "pub mod m;\n\npub fn one() -> u8 {\n    m::two()\n}\n");
    write(&io, "a/src/m.rs", "pub fn two() -> u8 {\n    2\n}\n");
    let (_, first) = engine_from(&io, 2, false);
    assert_eq!(first.files, engine_check::Files { read_back: 0, outlined: 3, probed: 3 }, "the first request outlines every file");
    write(&io, "a/src/m.rs", "pub fn two() -> u8 {\n    2\n}\n// a comment\n");
    let (now_ids, now) = engine_from(&io, 2, false);
    println!("a comment: files {:?}, pages {}, entered {}\n{}", now.files, now.pages_written, entries(&now), profile_lines(&io));
    assert_eq!(now.files, engine_check::Files { read_back: 2, outlined: 1, probed: 0 }, "one file outlined, its slot from the sweep; the others read back");
    assert_eq!(now.stages[blaze_engine::Stage::Outlines.index()].entered, 1, "one outline entered\n{}", entries(&now));
    write(&io, "a/src/m.rs", "pub fn two() -> u8 {\n    let _ = 0u8;\n    2\n}\n// a comment\n");
    let (body_ids, body) = engine_from(&io, 2, false);
    println!("a statement: files {:?}, pages {}, entered {}\n{}", body.files, body.pages_written, entries(&body), profile_lines(&io));
    assert_eq!(body.files, engine_check::Files { read_back: 2, outlined: 1, probed: 0 }, "the statement's file alone outlined");
    assert_eq!(body.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "one body entered, its library's and its test build's rows (D191: a row each unit that holds it)\n{}", entries(&body));
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    assert_eq!(fresh.files.outlined, 3, "a fresh run outlines every file");
    assert_eq!((body_ids, body.status.0, body.bodies.0.clone()), (fresh_ids, fresh.status.0, fresh.bodies.0.clone()), "the incremental answer is a fresh run's");
    assert!(now_ids.is_empty(), "the comment adds no finding: {now_ids:?}");
    // Nothing changed after the fresh run: the certificate, no file read.
    let (_, again) = engine_from(&io, 2, false);
    assert!(again.certified, "nothing moved: the certificate");
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
    assert_eq!(bodies, [(2 * BODIES as u32, 2 * BODIES as u32)], "a task a body's row, each entered once: the library's and its test build's (D191)");
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
    assert_eq!(bodies, [(4 * CRATES as u32, 4 * CRATES as u32)], "a task a body's row, each entered once: two bodies a crate, each under its library and its test build (D191)");
    let (notes, clean) = &checked.bodies;
    assert_eq!(*clean as usize, 2 * CRATES, "every body checked clean; notes: {:?}", &notes[..notes.len().min(3)]);
    // The Heads stage: one impl tables task a crate of the world, none refused.
    let (head_rows, head_refused, _) = checked.heads;
    println!("heads: {head_rows} rows, {head_refused} refused; reads of no task's record: {:?} ({}), of a head's signature: {}", checked.undefined_reads.0, checked.undefined_reads.1, checked.undefined_reads.2);
    assert!(head_rows as usize >= CRATES, "an impl tables task a crate: {head_rows}");
    assert_eq!(head_refused, 0, "no crate's impl tables refused");
    // Rule 21: every cell a task reads is defined by a task or ABSENT by its task's intent.
    assert_eq!(checked.undefined_reads.1, 0, "reads of a record no task has a row for: {:?}", checked.undefined_reads.0);
    // AN EDIT'S COST FOLLOWS THE EDIT (engine.md E6): one statement in one crate's body enters
    // one outline and one body, reads one file and reads every other outline back.
    write(&io, "c7/src/lib.rs", "pub fn f7() -> u8 {\n    let _ = c4::g4();\n    let _ = c5::g5();\n    let _ = c6::g6();\n    g7()\n}\n\npub fn g7() -> u8 {\n    let _ = 0u8;\n    2\n}\n");
    let began = std::time::Instant::now();
    let (_, edit) = engine_from(&io, 4, false);
    println!("one statement in {CRATES} crates: {:?}, files {:?}, pages written {}, entered:\n{}\n{}", began.elapsed(), edit.files, edit.pages_written, entries(&edit), profile_lines(&io));
    assert_eq!(edit.files, engine_check::Files { read_back: CRATES as u32 - 1, outlined: 1, probed: 0 }, "one file outlined and read");
    assert_eq!(edit.stages[blaze_engine::Stage::Outlines.index()].entered, 1, "one outline entered\n{}", entries(&edit));
    assert_eq!(
        edit.stages[blaze_engine::Stage::Bodies.index()].entered,
        2,
        "one body entered, in its 2 units' rows (D191: \"Every unit that holds a body checks it; the first holder in rank keeps the body's plain name (the base), each other holder's row is the name at its view\": c7's library and its test build)\n{}",
        entries(&edit)
    );
    // THE UNIT GRAPH IS READ BACK (plan row D2): no manifest moved, so the graph of 400 crates
    // is its stored record, no fetch run, no lock walked.
    assert_eq!(edit.units, crate::engine_units::Units { read_back: 1, discovered: 0, fetches_run: 0 }, "the graph read back\n{}", profile_lines(&io));
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
        let before = stdout(&io).len();
        let (_, checked) = engine(&io, 1);
        let said = stdout(&io).split_off(before);
        entered_once(&checked, 1);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of a macro-written fn (wrong {wrong}): {clean} clean, notes {notes:?}\n{said}");
        // The wrong form fails the command, the good one does not (the old checker was the
        // oracle until 2026-11-19).
        assert_eq!(checked.status.0 == Status::FAILED.0, wrong, "engine-check's status (wrong {wrong}):\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked")), "no body left unchecked: {notes:?}");
        // The wrong written body's error is printed (covered.rs `WRITTEN_UNPLACED`: at the
        // call's item, said so), not a note.
        assert!(notes.is_empty(), "no body's finding is a note: {notes:?}");
        assert_eq!(error_lines(&said).len(), usize::from(wrong), "the wrong written body's one error:\n{said}");
        assert!(error_lines(&said).iter().all(|line| line.contains("src/lib.rs:")), "the error is placed in the file:\n{said}");
        assert!(!said.contains("NOT checked: types"), "types checked whole:\n{said}");
    }
}

/// A `static` OR `const` DECLARED IN A BODY IS CHECKED, ONCE (handoff c13; tempo t1935d, 7 tests
/// NOT RUN "the body ids hold nothing for" chainspec's `mainnet::IDENTITY`, clap's
/// `augment_args::DEFAULT_VALUE` and `thread_local!`'s static). The Reference, "Block
/// expressions": an item in a block is an item, so its initializer is checked like a module's.
/// THE CAUSE (front2 every_body.rs `each_from`): only a body's local FNS were queued, so a local
/// static's initializer was checked by no task and no task's covers listed it, and a program
/// that read it found no row. Planted: `true` where a `u8` is written.
/// - the real shape: a static in an impl method's body. Before: 0 errors (never checked).
/// - TWIN, one check: a const in a block that is a module (it holds an impl): its body is listed
///   (covered.rs `in_blocks`) AND it is its body's local. With only `each_from` changed it would
///   print 2 errors; `in_blocks` leaves it to the local walk, so 1.
/// - CONTROL: the same static written right: no error.
#[test]
fn engine_check_checks_a_static_or_const_declared_in_a_body_once() {
    let method = "pub struct S;\n\nimpl S {\n    pub fn get() -> u8 {\n        static ONE: u8 = true;\n        ONE\n    }\n}\n";
    let block = "pub fn outer() -> u8 {\n    struct T;\n    impl T {\n        fn m() {}\n    }\n    const FOUR: u8 = true;\n    FOUR\n}\n";
    let right = "pub struct S;\n\nimpl S {\n    pub fn get() -> u8 {\n        static ONE: u8 = 1;\n        ONE\n    }\n}\n";
    for (case, text, errors) in [("a static in a method", method, 1usize), ("a const in a block module", block, 1), ("control: written right", right, 0)] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", text);
        let before = stdout(&io).len();
        let (_, checked) = engine(&io, 1);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked")), "{case}: no body left unchecked: {notes:?}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: the planted initializer's error, printed once:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
        assert_eq!(checked.status.0 == Status::FAILED.0, errors > 0, "{case}: the command's status:\n{said}");
    }
}

/// What tracing's callsite statics expand with. The simulated shelf's `core` declares no
/// builtin macro: the crate declares the three as core does (`#[rustc_builtin_macro]`), and
/// `__macro_support` re-exports its `concat` as tracing's re-exports `::core::concat` (the
/// `$crate::__macro_support::concat!` shape kept).
pub(super) const CALLSITE_SUPPORT: &str = "#![feature(rustc_attrs)]\n\n#[rustc_builtin_macro]\n#[macro_export]\nmacro_rules! concat {\n    ($($t:tt)*) => { /* compiler built-in */ };\n}\n\n#[rustc_builtin_macro]\n#[macro_export]\nmacro_rules! file {\n    () => { /* compiler built-in */ };\n}\n\n#[rustc_builtin_macro]\n#[macro_export]\nmacro_rules! line {\n    () => { /* compiler built-in */ };\n}\n\n#[doc(hidden)]\npub mod __macro_support {\n    pub use crate::concat;\n}\n\n";

/// TRACING'S CALLSITE STATIC CHECKS (tempo rt-loop a2000: 394 items not checked, "HostError
/// Place, the call of `$crate :: __macro_support :: concat!`" in every workspace body with a
/// tracing event). `event!` writes, in the user's fn, `static META = .. concat!(file!(), ":",
/// line!()) ..` through its `$crate::__macro_support` re-export. THE CAUSE: since the test
/// above, a local static's initializer is a body of its own (every_body.rs), and its host was
/// placed at the LOCAL key (front2 body.rs `BodyHost::of(.., item, ..)`), which the names do
/// not place, so `line!()` in that eager input was refused. Now placed at its enclosing fn,
/// at the outermost call for a static a macro wrote (body.rs `body_at`).
/// - the real shape: the callsite static an `event!` writes: clean.
/// - TWIN, planted: the same static with `true` where a `u8` is written: ONE E0308, so the
///   initializer was checked past its `line!()` (refused, it was "not checked" instead).
/// - TWIN: the same initializer written by hand in the fn (a written local, its own byte): clean.
/// - CONTROL: at module level (the item host, never refused): clean.
#[test]
fn engine_check_checks_a_callsite_static_with_line_in_eager_input() {
    let support = CALLSITE_SUPPORT;
    let event = |init: &str| {
        std::format!("{support}#[macro_export]\nmacro_rules! event {{\n    () => {{{{\n        static META: (&str, u8) = ($crate::__macro_support::concat!(file!(), \":\", line!()), {init});\n        META.1\n    }}}};\n}}\n\npub fn handle_tip() -> u8 {{\n    event!()\n}}\n")
    };
    let real = event("1");
    let planted = event("true");
    let by_hand = std::format!("{support}pub fn handle_tip() -> u8 {{\n    static META: (&str, u8) = (crate::__macro_support::concat!(file!(), \":\", line!()), 1);\n    META.1\n}}\n");
    let module = std::format!("{support}static META: (&str, u8) = (crate::__macro_support::concat!(file!(), \":\", line!()), 1);\n\npub fn handle_tip() -> u8 {{\n    META.1\n}}\n");
    let cases = [("the callsite static an `event!` writes", &real, 0usize), ("planted: `true` for a `u8`", &planted, 1), ("written by hand in the fn", &by_hand, 0), ("control: at module level", &module, 0)];
    for (case, text, errors) in cases {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", text);
        let before = stdout(&io).len();
        let mut o = opts(&io, 1, true);
        o.toolchain = lang_shelf(&io);
        let (_, checked) = engine_opts(&io, o);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked") && !note.contains("HostError")), "{case}: no body left unchecked: {notes:?}");
        assert!(!said.contains("HostError"), "{case}: `line!()` placed, never refused:\n{said}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: errors printed:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
    }
}

/// `line!()` IN A FN A MACRO WROTE AT MODULE LEVEL IS THE CALL'S LINE (tempo a2015 and t2025d:
/// 8590 lines "HostError { ask: Place, why: Refused }", tracing's `concat!(file!(), ":",
/// line!())` in items a macro wrote: pin-project's among them). Core `line!`: "the first macro
/// invocation leading up to the expansion of the `line!` macro". THE CAUSE: a body's place
/// (body_host.rs `Placed::of`) was its item's first byte in a FILE, and an item a call wrote is
/// in the call's expansion: no place, `line!()` refused. Now by the rule a `#[track_caller]`
/// `Location` is placed by (program/lazy seam_rules.rs `body_place`): the outermost call.
/// The value is asserted through a const: `[0; LINE as usize]` typed `[u8; K]`.
/// - the real shape: `handler!()` writes `handle_tip` with tracing's callsite static and
///   `const LINE: u32 = line!()`, K the line of `handler!();`: clean.
/// - TWIN, planted: K one past that line: ONE E0308, so the value was checked (not refused).
/// - CONTROL: the same fn written by hand, K the line of its own `line!()`: clean.
#[test]
fn engine_check_places_line_in_a_fn_a_macro_wrote_at_its_call() {
    let line_of = |text: &str, starts: &str| text.lines().position(|l| l.trim_start().starts_with(starts)).expect("the marked line") + 1;
    let body = |k: &str| {
        std::format!("pub fn handle_tip() -> u8 {{\n    static META: (&str, u8) = ($crate::__macro_support::concat!(file!(), \":\", line!()), 1);\n    const LINE: u32 = line!();\n    let _at: [u8; {k}] = [0; LINE as usize];\n    META.1\n}}\n")
    };
    // `K` written as a placeholder, then replaced by the line found in the whole text.
    let written = |shift: usize| {
        let text = std::format!("{CALLSITE_SUPPORT}macro_rules! handler {{\n    () => {{\n{}    }};\n}}\n\nhandler!();\n", body("K"));
        let at = line_of(&text, "handler!();") + shift;
        text.replace("[u8; K]", &std::format!("[u8; {at}]"))
    };
    let real = written(0);
    let planted = written(1);
    let by_hand = {
        let text = std::format!("{CALLSITE_SUPPORT}{}", body("K").replace("$crate", "crate"));
        let at = line_of(&text, "const LINE");
        text.replace("[u8; K]", &std::format!("[u8; {at}]"))
    };
    let cases = [("the fn `handler!` writes", &real, 0usize), ("planted: one line past the call", &planted, 1), ("control: written by hand", &by_hand, 0)];
    for (case, text, errors) in cases {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", text);
        let before = stdout(&io).len();
        let mut o = opts(&io, 1, true);
        o.toolchain = lang_shelf(&io);
        let (_, checked) = engine_opts(&io, o);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked") && !note.contains("HostError")), "{case}: no body left unchecked: {notes:?}");
        assert!(!said.contains("HostError"), "{case}: `line!()` placed, never refused:\n{said}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: errors printed:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
    }
}

/// tracing-0.1.44's and tracing-core-0.1.36's macros an `event!` with a target and no fields
/// expands through, VERBATIM, reduced by deleting arms (src/macros.rs `event!` (the target
/// arm), `callsite2!` (its third arm), `level_enabled!`, `fieldset!` (base and entry),
/// `valueset_all!` (base and the no-fields entry), `__tracing_log!` (the arm without `log`,
/// which writes nothing, so `valueset_all!` is never expanded); tracing-core lib.rs
/// `identify_callsite!`,
/// `metadata!` (its second arm)). One cut inside an arm: the `if enabled` branch's body, a
/// closure called, emptied, because the fixture's core declares no `Fn` lang item to call it
/// by. The types are the smallest that keep each call's shape (`Level` a module of `u8`
/// consts, so `level_enabled!`'s `<=` is the builtin one the fixture's core has); the builtin
/// macros are declared as core declares them (as [`CALLSITE_SUPPORT`]).
const TRACING_LIB: &str = r#"#![feature(rustc_attrs)]
#![allow(non_snake_case)]

#[rustc_builtin_macro]
#[macro_export]
macro_rules! concat {
    ($($t:tt)*) => { /* compiler built-in */ };
}

#[rustc_builtin_macro]
#[macro_export]
macro_rules! file {
    () => { /* compiler built-in */ };
}

#[rustc_builtin_macro]
#[macro_export]
macro_rules! line {
    () => { /* compiler built-in */ };
}

#[rustc_builtin_macro]
#[macro_export]
macro_rules! module_path {
    () => { /* compiler built-in */ };
}

#[doc(hidden)]
pub mod __macro_support {
    pub use crate::concat;
    pub use crate::file;
    pub use crate::line;
    pub use crate::module_path;
    pub use crate::callsite::Callsite;

    pub enum Option<T> {
        None,
        Some(T),
    }

    pub fn __is_enabled(_meta: &crate::Metadata<'static>, _interest: crate::callsite::Interest) -> bool {
        true
    }
}

pub mod Level {
    pub const WARN: u8 = 2;
}

pub mod level_filters {
    pub const STATIC_MAX_LEVEL: u8 = 5;

    pub mod LevelFilter {
        pub fn current() -> u8 {
            5
        }
    }
}

pub use metadata::Metadata;

pub mod metadata {
    pub struct Kind(pub u8);

    impl Kind {
        pub const EVENT: Kind = Kind(1);
    }

    pub struct Metadata<'a> {
        pub name: &'static str,
        pub target: &'a str,
        pub level: u8,
        pub file: crate::__macro_support::Option<&'a str>,
        pub line: crate::__macro_support::Option<u32>,
        pub module_path: crate::__macro_support::Option<&'a str>,
        pub fields: crate::field::FieldSet,
        pub kind: Kind,
    }

    impl<'a> Metadata<'a> {
        pub const fn new(
            name: &'static str,
            target: &'a str,
            level: u8,
            file: crate::__macro_support::Option<&'a str>,
            line: crate::__macro_support::Option<u32>,
            module_path: crate::__macro_support::Option<&'a str>,
            fields: crate::field::FieldSet,
            kind: Kind,
        ) -> Self {
            Metadata { name, target, level, file, line, module_path, fields, kind }
        }
    }
}

pub mod field {
    pub struct FieldSet {
        pub names: &'static [&'static str; 0],
        pub callsite: crate::callsite::Identifier,
    }

    impl FieldSet {
        pub const fn new(names: &'static [&'static str; 0], callsite: crate::callsite::Identifier) -> Self {
            FieldSet { names, callsite }
        }
    }
}

pub mod callsite {
    pub trait Callsite {
        fn metadata(&self) -> &crate::Metadata<'static>;
    }

    pub struct Identifier(pub &'static DefaultCallsite);

    pub struct Interest(pub u8);

    impl Interest {
        pub fn is_never(&self) -> bool {
            self.0 == 0
        }
    }

    pub struct DefaultCallsite {
        pub meta: &'static crate::Metadata<'static>,
    }

    impl DefaultCallsite {
        pub const fn new(meta: &'static crate::Metadata<'static>) -> Self {
            DefaultCallsite { meta }
        }

        pub fn interest(&self) -> Interest {
            Interest(1)
        }
    }

    impl Callsite for DefaultCallsite {
        fn metadata(&self) -> &crate::Metadata<'static> {
            self.meta
        }
    }
}

#[macro_export]
macro_rules! event {
    // Target.
    (target: $target:expr, $lvl:expr, { $($fields:tt)* } )=> ({
        use $crate::__macro_support::Callsite as _;
        static __CALLSITE: $crate::callsite::DefaultCallsite = $crate::callsite2! {
            name: $crate::__macro_support::concat!(
                "event ",
                $crate::__macro_support::file!(),
                ":",
                $crate::__macro_support::line!()
            ),
            kind: $crate::metadata::Kind::EVENT,
            target: $target,
            level: $lvl,
            fields: $($fields)*
        };
        let enabled = $crate::level_enabled!($lvl) && {
            let interest = __CALLSITE.interest();
            !interest.is_never() && $crate::__macro_support::__is_enabled(__CALLSITE.metadata(), interest)
        };
        if enabled {
        } else {
            $crate::__tracing_log!(
                $lvl,
                __CALLSITE,
                &$crate::valueset_all!(__CALLSITE.metadata().fields(), $($fields)*)
            );
        }
    });
}

/// Constructs a new static callsite for a span or event.
#[doc(hidden)]
#[macro_export]
macro_rules! callsite2 {
    (
        name: $name:expr,
        kind: $kind:expr,
        target: $target:expr,
        level: $lvl:expr,
        fields: $($fields:tt)*
    ) => {{
        static META: $crate::Metadata<'static> = {
            $crate::metadata! {
                name: $name,
                target: $target,
                level: $lvl,
                fields: $crate::fieldset!( $($fields)* ),
                callsite: &__CALLSITE,
                kind: $kind,
            }
        };
        $crate::callsite::DefaultCallsite::new(&META)
    }};
}

#[macro_export]
// TODO: determine if this ought to be public API?`
#[doc(hidden)]
macro_rules! level_enabled {
    ($lvl:expr) => {
        $lvl <= $crate::level_filters::STATIC_MAX_LEVEL
            && $lvl <= $crate::level_filters::LevelFilter::current()
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! fieldset {
    // == base case ==
    (@ { $(,)* $($out:expr),* $(,)* } $(,)*) => {
        &[ $($out),* ]
    };

    // == entry ==
    ($($args:tt)*) => {
        $crate::fieldset!(@ { } $($args)*,)
    };

}

#[doc(hidden)]
#[macro_export]
macro_rules! valueset_all {

    // === base case ===
    (@ { $(,)* $($val:expr),* $(,)* } $(,)*) => {
        &[ $($val),* ]
    };

    ($fields:expr,) => {
        {
            $fields.value_set_all(&[])
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __tracing_log {
    ($level:expr, $callsite:expr, $value_set:expr) => {};
}

#[doc(hidden)]
#[macro_export]
macro_rules! identify_callsite {
    ($callsite:expr) => {
        $crate::callsite::Identifier($callsite)
    };
}

#[macro_export]
macro_rules! metadata {
    (
        name: $name:expr,
        target: $target:expr,
        level: $level:expr,
        fields: $fields:expr,
        callsite: $callsite:expr,
        kind: $kind:expr,
    ) => {
        $crate::metadata::Metadata::new(
            $name,
            $target,
            $level,
            $crate::__macro_support::Option::Some($crate::__macro_support::file!()),
            $crate::__macro_support::Option::Some($crate::__macro_support::line!()),
            $crate::__macro_support::Option::Some($crate::__macro_support::module_path!()),
            $crate::field::FieldSet::new($fields, $crate::identify_callsite!($callsite)),
            $kind,
        )
    };
}
"#;

/// A workspace crate's bodies with one tracing event each (`TARGET` replaced by the event's
/// target): an inherent method (tempo's `async fn handle_tip(&self, tip: FinalizedTip)`,
/// crates/consensus/src/feed/actor.rs:64, made sync: the fixture's core has no `Future`), a
/// trait impl's method, and a free fn.
const CONSENSUS_LIB: &str = r#"pub struct Actor {
    pub n: u8,
}

impl Actor {
    pub fn handle_tip(&self, tip: u8) -> u8 {
        tracing::event!(target: TARGET, tracing::Level::WARN, {});
        tip + self.n
    }
}

pub trait Handle {
    fn handle(&self) -> u8;
}

impl Handle for Actor {
    fn handle(&self) -> u8 {
        tracing::event!(target: "consensus::feed", tracing::Level::WARN, {});
        self.n
    }
}

pub fn now_millis(at: u8) -> u8 {
    tracing::event!(target: "consensus::feed", tracing::Level::WARN, {});
    at
}
"#;

/// TRACING'S EVENT IN A METHOD CHECKS (tempo rt-loop a2210: 167 bodies "lowering refused ..
/// HostError Place, the call of `$crate :: __macro_support :: concat!`", every one a method;
/// none of tempo's 124 free fns with an event refused). The event's `static META` is a
/// copied local two levels in (in `__CALLSITE`'s initializer, in the method), and front2
/// body.rs `body_at` walks out to the method's key. THE CAUSE (from reading): a member's
/// library row was found by `library_body`'s same-slice test against the head's text, which
/// a seeded member's head does not share with the names' slice: no row, no place, `line!()`
/// refused. Now by keys.rs `source_member` (container, kind, name). The real macros
/// ([`TRACING_LIB`]) from a path dependency, in a workspace package (`file!()` answered):
/// - the real shape: an inherent method, a trait impl's method and a free fn: clean, nothing
///   refused, no body left unchecked.
/// - TWIN, planted: the method's event's target `7u8` for `&str`: ONE E0308, so META's
///   initializer was lowered past its `line!()` and checked.
/// - CONTROL: the free fn alone was placed before the change too (its `library_in` row);
///   rule 23: with body_at's member arm removed, the real shape refuses the two methods only.
#[test]
fn engine_check_checks_a_tracing_event_in_a_method() {
    let cases = [("an event in a method, a trait method and a free fn", "\"consensus::feed\"", 0usize), ("planted: the method's target `7u8`", "7u8", 1)];
    for (case, target, errors) in cases {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"tracing\", \"crates/consensus\"]\nresolver = \"2\"\n");
        write(&io, "tracing/Cargo.toml", "[package]\nname = \"tracing\"\nversion = \"0.1.44\"\nedition = \"2018\"\n");
        write(&io, "tracing/src/lib.rs", TRACING_LIB);
        write(&io, "crates/consensus/Cargo.toml", "[package]\nname = \"consensus\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ntracing = { path = \"../../tracing\" }\n");
        write(&io, "crates/consensus/src/lib.rs", &CONSENSUS_LIB.replace("TARGET", target));
        let before = stdout(&io).len();
        let mut o = opts(&io, 1, true);
        o.toolchain = lang_shelf(&io);
        let (_, checked) = engine_opts(&io, o);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked") && !note.contains("HostError")), "{case}: no body left unchecked: {notes:?}");
        assert!(!said.contains("HostError") && !said.contains("could not check"), "{case}: `line!()` placed, never refused:\n{said}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: errors printed:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
    }
}

/// A METHOD'S BLOCK IMPORT IS READ (front2 locals.rs `library_item_or_member`). A body's
/// blocks the names library made modules are found by the library's key of the body: for a
/// method that key was found by its text's slice, which a seeded member's text is not (the
/// cause of the test above), so a method's blocks had no modules: a macro its block imports
/// was not found there, and `Locals::keep` and glob_uses.rs `PatternBlocks::of` read no block
/// module either. Now one lookup, by container, kind and name, for all three.
/// - the real shape: an inherent method whose block imports a macro (`use crate::m::seven;`,
///   a `macro_rules!` of a module, re-exported by `use`) and calls it, and one whose block
///   globs an enum's variants for a `match`: clean, nothing refused.
/// - TWIN, planted: the macro's value (`7`) where a `bool` is written: ONE E0308, so the call
///   was expanded through the block's import and checked.
/// - CONTROL: the same block in a free fn (its body found by its item before the change too):
///   clean.
#[test]
fn engine_check_reads_a_block_import_in_a_method() {
    let support = "mod m {\n    macro_rules! seven {\n        () => {\n            7\n        };\n    }\n    pub(crate) use seven;\n}\n\npub enum E {\n    A,\n    B,\n}\n\n";
    let method = |ty: &str| {
        std::format!(
            "{support}pub struct Actor;\n\nimpl Actor {{\n    pub fn get() -> u8 {{\n        let n: {ty} = {{\n            use crate::m::seven;\n            seven!()\n        }};\n        let _ = n;\n        1\n    }}\n\n    pub fn pick() -> u8 {{\n        let e = E::A;\n        {{\n            use crate::E::*;\n            match e {{\n                A => 1,\n                B => 2,\n            }}\n        }}\n    }}\n}}\n"
        )
    };
    let real = method("u8");
    let planted = method("bool");
    let free = std::format!("{support}pub fn get() -> u8 {{\n    let n: u8 = {{\n        use crate::m::seven;\n        seven!()\n    }};\n    n\n}}\n");
    let cases = [("a method's block imports a macro and globs variants", &real, 0usize), ("planted: `bool` for the macro's `7`", &planted, 1), ("control: the block in a free fn", &free, 0)];
    for (case, text, errors) in cases {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", text);
        let before = stdout(&io).len();
        let mut o = opts(&io, 1, true);
        o.toolchain = lang_shelf(&io);
        let (_, checked) = engine_opts(&io, o);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked") && !note.contains("refused")), "{case}: no body left unchecked or refused: {notes:?}");
        assert!(!said.contains("could not check") && !said.contains("lowering refused"), "{case}: every body lowered:\n{said}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: errors printed:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
    }
}

/// `include_str!` AND `file!()` IN A MEMBER ARE READ FROM ITS CONTAINER'S FILE (front2
/// body_host.rs `body_file`). tempo rt-loop a2210: `HostError Include` in four members of
/// crates/node/tests/it/utils.rs and zones' (`new`, `reference_genesis`, `launch`, the
/// associated const `TESTNET_REFERENCE_GENESIS`), in none of the same crate's free fns. A
/// member is placed at its member row (body.rs `body_at`), and the items record has no row
/// for a member, so it had no file. Now its container's.
/// - the real shape: a method and an associated const reading `include_str!("../assets/g.json")`,
///   and a method returning `file!()`: clean, nothing refused.
/// - TWIN, planted: the method's include typed `u8`: ONE E0308, so the include was read.
/// - CONTROL: the same include in a free fn: clean.
#[test]
fn engine_check_reads_an_include_in_a_member() {
    let support = "#![feature(rustc_attrs)]\n\n#[rustc_builtin_macro]\nmacro_rules! include_str {\n    ($($t:tt)*) => { /* compiler built-in */ };\n}\n\n#[rustc_builtin_macro]\nmacro_rules! file {\n    () => { /* compiler built-in */ };\n}\n\n";
    let member = |ty: &str| {
        std::format!(
            "{support}pub enum ForkSchedule {{\n    Testnet,\n}}\n\nimpl ForkSchedule {{\n    const TESTNET_REFERENCE_GENESIS: &'static str = include_str!(\"../assets/g.json\");\n\n    pub fn reference_genesis(&self) -> {ty} {{\n        include_str!(\"../assets/g.json\")\n    }}\n\n    pub fn testnet() -> &'static str {{\n        Self::TESTNET_REFERENCE_GENESIS\n    }}\n\n    pub fn at() -> &'static str {{\n        file!()\n    }}\n}}\n"
        )
    };
    let real = member("&'static str");
    let planted = member("u8");
    let free = std::format!("{support}pub fn reference_genesis() -> &'static str {{\n    include_str!(\"../assets/g.json\")\n}}\n");
    let cases = [("a method, an associated const and `file!()` in a method", &real, 0usize), ("planted: the include typed `u8`", &planted, 1), ("control: the include in a free fn", &free, 0)];
    for (case, text, errors) in cases {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
        write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "a/src/lib.rs", text);
        write(&io, "a/assets/g.json", "{\"n\":1}\n");
        let before = stdout(&io).len();
        let mut o = opts(&io, 1, true);
        o.toolchain = lang_shelf(&io);
        let (_, checked) = engine_opts(&io, o);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        println!("engine-check of {case}: {clean} clean, notes {notes:?}\n{said}");
        assert!(notes.iter().all(|note| !note.contains("not checked") && !note.contains("HostError")), "{case}: no body left unchecked: {notes:?}");
        assert!(!said.contains("HostError") && !said.contains("could not check"), "{case}: the include and `file!()` read, never refused:\n{said}");
        let found = error_lines(&said);
        assert_eq!(found.len(), errors, "{case}: errors printed:\n{said}");
        assert!(found.iter().all(|line| line.contains("E0308")), "{case}: a mismatched type (E0308):\n{said}");
    }
}

#[test]
fn engine_check_lists_what_check_lists_for_the_outlines() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    workspace(&io);
    let mut listed = std::vec::Vec::new();
    for workers in [1, 4] {
        let (ids, checked) = engine(&io, workers);
        // The stray `}` of `src/a/b.rs`, a syntax error (the old checker was the oracle until
        // 2026-11-19); 1 and 4 workers list the same.
        assert!(ids.iter().any(|(code, _)| code == "syntax"), "engine-check with {workers} worker(s) listed no syntax error: {ids:?}");
        listed.push(ids);
        // One `Outlines` task a file (lib.rs, a.rs, a/b.rs), each entered once; every phase
        // entered what it planned.
        let outlines: std::vec::Vec<_> = checked.phases.iter().filter(|(phase, ..)| phase.stage == blaze_engine::Stage::Outlines).map(|(_, planned, entered)| (*planned, *entered)).collect();
        assert_eq!(outlines, [(3, 3)], "outline tasks planned and entered with {workers} worker(s)");
        entered_once(&checked, workers);
        assert_eq!(checked.status.0, Status::FAILED.0, "a syntax error fails the command");
    }
    assert_eq!(listed[0], listed[1], "1 and 4 workers list the same findings");
}

/// Each stage's entries and the "entered because" table of one request, for a message.
pub(super) fn entries(checked: &engine_check::Checked) -> std::string::String {
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
    // A comment at the end of one file: its file's outline, named by its path (it stands while
    // its text moves), entered because its key moved; and the file's own lint unit (unit 0, the
    // file with its top-level items as holes, engine_lints.rs THE PLAN), whose bytes hold the
    // comment, so a token rule over comments sees it. Its surface cell stands, so no crate's
    // names, no body and no item's lint unit is entered.
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n// a comment\n");
    let (_, comment) = engine_from(&io, 2, false);
    assert_eq!(because(&comment), [(blaze_engine::Stage::Outlines, "key moved", None, 1), (blaze_engine::Stage::Lints, "key moved", None, 1)], "a comment enters its file's outline and its file's lint unit alone: {}", entries(&comment));
    assert_eq!(comment.entered, 2, "a comment: {}", entries(&comment));
    nothing("a comment");

    // A statement added to one body: that file's outline, that one body, and the floor of the
    // head tasks: the one head task of its item (keyed by the item's whole text, engine_heads.rs
    // `plan_items`), which makes equal heads, so no reader moves.
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    let two = 2;\n    two\n}\n// a comment\n");
    let (_, statement) = engine_from(&io, 2, false);
    assert_eq!(
        because(&statement),
        // The body's two rows (D191: its library's and its test build's), and the file's ONE
        // lint task (engine_lints.rs `plan`: a task a file, its key moved by the item's unit's,
        // the one unit it relints; the file's unit 0 holds the item as one mark).
        [(blaze_engine::Stage::Outlines, "key moved", None, 1), (blaze_engine::Stage::Bodies, "key moved", None, 2), (blaze_engine::Stage::Lints, "key moved", None, 1)],
        "a statement enters its outline, its body's two rows (D191) and its lint units alone: {}",
        entries(&statement)
    );
    assert_eq!(statement.stages[blaze_engine::Stage::Heads.index()].identical, 0, "a statement enters no heads: {}", entries(&statement));
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

/// `blaze why` (engine_why.rs): after `a::f` is made private, `why` of `b`'s body `h`, which
/// finds `f` through its glob of `a`'s names, prints the chain back to the edit: the body
/// entered for a read that moved (`f`'s binding cell), each task that defined the moved cell,
/// and last the edited file's outline, entered because its key moved. (It was an item added to
/// `a`'s root; with a head task per item and names read by cell, `h` reads nothing that edit
/// moves and is not entered, lane EY.) CONTROL: `why` of the file alone is that last step; an
/// item the file does not have is said, not guessed.
#[test]
fn engine_why_walks_back_to_the_edit() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    two_crates(&io);
    write(&io, "b/src/lib.rs", "use a::*;\n\npub fn h() -> u8 {\n    f() + m::k()\n}\n");
    let _ = engine_from(&io, 2, false);
    write(&io, "a/src/lib.rs", "pub mod m;\n\nfn f() -> u8 {\n    1\n}\n");
    let (_, item) = engine_from(&io, 2, false);
    let said = why(&io, "b/src/lib.rs::h");
    let lines: std::vec::Vec<&str> = said.lines().collect();
    assert!(lines.len() >= 2, "a chain of steps: {said}\n{}", entries(&item));
    // The label now ends in the edition the body was read under ("(read as edition 2024, ..)"), before the colon.
    assert!(lines[0].starts_with("the body of `b/src/lib.rs::h`") && lines[0].contains(": entered in request") && lines[0].contains("a read moved"), "first, the body and its moved read: {said}");
    let last = lines[lines.len() - 1];
    assert!(last.starts_with("`a/src/lib.rs`'s outline: entered in request") && last.contains("its key moved"), "last, the edited file: {said}");
    let file = why(&io, "a/src/lib.rs");
    assert_eq!(file.lines().collect::<std::vec::Vec<_>>(), [last], "the file's own why is the last step");
    assert!(why(&io, "a/src/lib.rs::nothing").contains("has no item `nothing`"), "an item the file does not have is said");
}

/// One request's listed findings, its printed lines but the closing one (it holds the time and
/// the count entered), and its counts.
pub(super) fn said(io: &Io, fresh: bool) -> (std::vec::Vec<(std::string::String, std::string::String)>, std::vec::Vec<std::string::String>, engine_check::Checked) {
    let before = stdout(io).len();
    let (ids, checked) = engine_from(io, 2, fresh);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    (ids, lines, checked)
}

/// The lines of `said` that print an error finding (`error[<code>]`), in order.
pub(super) fn error_lines(said: &str) -> std::vec::Vec<std::string::String> {
    said.lines().filter(|line| line.contains("error[")).map(str::to_owned).collect()
}

/// The lines of `said` that print a warning finding (`warning[<code>]`), in order.
fn warning_lines(said: &str) -> std::vec::Vec<std::string::String> {
    said.lines().filter(|line| line.contains("warning[")).map(str::to_owned).collect()
}

/// The last line printed: the last command's closing line.
fn closing(io: &Io) -> std::string::String {
    stdout(io).lines().last().unwrap_or("").to_owned()
}

/// The listed findings that are placed (an id shown), sorted: what two commands compare.
pub(super) fn placed_ids(ids: &[(std::string::String, std::string::String)]) -> std::vec::Vec<(std::string::String, std::string::String)> {
    let mut placed: std::vec::Vec<_> = ids.iter().filter(|(_, id)| !id.is_empty()).cloned().collect();
    placed.sort();
    placed
}

/// A BODY'S ERROR IS PRINTED AS `check` PRINTS IT (the real run g125544 printed "the body of
/// `mean` has 1 finding(s) ... not carried out" notes over "0 errors: blaze check"): one type
/// error in one body is the one `error[` line `check` prints, the closing line says "1 error"
/// and the command fails as `check` does; with nothing changed the certificate prints it again
/// from the stored lines with no body entered; a comment line above it moves it a line down as
/// a fresh run does (the stored finding is placed by its item's chain, never a stored line);
/// fixed, no error is printed and the status is a clean fresh run's. CONTROL: before, the
/// engine printed no `error[` line here and "0 errors".
#[test]
fn engine_check_prints_a_body_error_as_check_does() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    typed_crates(&io, Typed::Wrong);
    let (ids, first, checked) = said(&io, false);
    let first_said = first.join("\n");
    let first_closing = closing(&io);
    println!("engine-check printed:\n{first_said}\n{first_closing}");
    // `wrong`'s `true` where a `u8` is returned, at its line in `b` (the old checker was this
    // test's oracle until 2026-11-19; the place is the oracle now).
    let errors = error_lines(&first_said);
    assert_eq!(errors.len(), 1, "one error printed:\n{first_said}");
    assert!(errors[0].contains("b/src/lib.rs:6:"), "the error at `true`'s line in `wrong`:\n{first_said}");
    assert!(first_closing.contains("1 error"), "the closing line counts it: {first_closing}");
    assert_eq!(checked.status.0, Status::FAILED.0, "a type error fails the command");
    assert_eq!(placed_ids(&ids).len(), 1, "one placed finding: {ids:?}");

    // (2) Nothing changed: the certificate, the same lines from the stored answer.
    let (again_ids, again, certificate) = said(&io, false);
    assert!(certificate.certified && certificate.entered == 0, "nothing changed: no task entered; {}", entries(&certificate));
    assert_eq!((again_ids, again), (ids.clone(), first.clone()), "the certificate prints the first request's lines");
    assert!(closing(&io).contains("1 error"), "and counts the error again: {}", closing(&io));
    assert_eq!(certificate.status.0, checked.status.0, "and exits alike");

    // A comment line above the wrong body: printed where a fresh run prints it.
    let wrong = "pub fn h() -> u8 {\n    a::one() + 1\n}\n\npub fn wrong() -> u8 {\n    true\n}\n";
    write(&io, "b/src/lib.rs", &format!("// a comment\n{wrong}"));
    let (moved_ids, moved, moved_checked) = said(&io, false);
    let (fresh_ids, fresh, _) = said(&io, true);
    println!("a comment above: entered {}, bodies {}\n{}", moved_checked.entered, moved_checked.stages[blaze_engine::Stage::Bodies.index()].entered, moved.join("\n"));
    assert_eq!((moved_ids, error_lines(&moved.join("\n"))), (fresh_ids, error_lines(&fresh.join("\n"))), "after a comment the error is printed as a fresh run prints it\n{}", entries(&moved_checked));
    assert_ne!(error_lines(&moved.join("\n")), error_lines(&first_said), "control: the comment moved the error's line");

    // (3) Fixed: no error, the status a clean fresh run's.
    typed_crates(&io, Typed::Plain);
    let (fixed_ids, fixed, fixed_checked) = said(&io, false);
    let fixed_closing = closing(&io);
    let (clean_ids, _, clean) = said(&io, true);
    assert!(error_lines(&fixed.join("\n")).is_empty(), "fixed: no error printed:\n{}", fixed.join("\n"));
    assert!(!fixed_closing.contains("error"), "fixed: the closing line counts no error: {fixed_closing}");
    assert_eq!((fixed_ids, fixed_checked.status.0), (clean_ids, clean.status.0), "fixed: as a clean fresh run");
}

/// A REFUSED BODY STANDS ON WHAT ITS REFUSAL SAW (plan row A8): `b` calls into `a`, a member
/// proc macro whose names cannot be made at all (the test shelf has no `wasm32-wasip1` side,
/// names/scope.rs `prepare`'s refused crate, as `failed_dep`), so `b`'s body is refused with a
/// note naming why. (A syntax error in `a` no longer does it: `a` still writes its names record
/// with the finding in it, and `b` is checked, `an_unresolved_import_stops_only_what_reads_it`.)
/// With nothing changed the refusal stands on its reads (each crate's names cell, the graph's):
/// no body task is entered again. Fixed (`a` a plain library), the graph's cell and `a`'s names
/// cell move, and exactly the refused bodies, `one` and `h`, are entered and checked clean.
/// CONTROL: the fixture's `a` really failed (its failure is printed); before `failed_with`,
/// every refusal read nothing, so it was owed and entered again by each request.
#[test]
fn engine_check_keeps_a_refused_body_until_what_it_read_moves() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    typed_crates(&io, Typed::Plain);
    let plain = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    write(&io, "a/Cargo.toml", &format!("{plain}\n[lib]\nproc-macro = true\n"));
    // (1) The member's body is refused by name, never checked under a scope without `a`.
    let (_, first_lines, first) = said(&io, false);
    let (notes, _) = &first.bodies;
    println!("a broken dependency: {}, notes {notes:?}\n{}", entries(&first), first_lines.join("\n"));
    assert!(first_lines.iter().any(|line| line.contains("crate `a`")), "control: `a`'s names failed, naming it (else the fixture fails nothing):\n{}", first_lines.join("\n"));
    let refused: std::vec::Vec<&std::string::String> = notes.iter().filter(|note| note.starts_with("`b/src/lib.rs`") && note.contains("not checked because")).collect();
    assert_eq!(refused.len(), 1, "`b`'s one body is refused with a note: {notes:?}\n{}", entries(&first));
    assert!(
        refused[0].contains("no names record") || refused[0].contains("no `Names` task") || refused[0].contains("check scope could not be read"),
        "the note says its scope's names are missing: {}",
        refused[0]
    );

    // (2) Nothing changed: no task entered, the refusal stands on its reads. (The certificate
    // standing WITH `a`'s failure, its stored notes and lines printed again, is plan row D6's
    // assertion: until "owed" is exact and a certificate may hold findings, a kept finding
    // means no certificate; as `a_failed_dependency_leaves_its_dependents_solved`.)
    let (_, again_lines, again) = said(&io, false);
    println!("nothing changed: {}\n{}", entries(&again), again_lines.join("\n"));
    assert_eq!(again.stages[blaze_engine::Stage::Bodies.index()].entered, 0, "a refusal with nothing changed enters no body: {}", entries(&again));
    assert_eq!(again.entered, 0, "nothing changed: no task entered, each failed row stands by its reads; {}", entries(&again));

    // (3) Fixed: exactly the refused bodies are entered, and each checks clean.
    write(&io, "a/Cargo.toml", plain);
    let (_, fixed_lines, fixed) = said(&io, false);
    println!("fixed: {}\n{}", entries(&fixed), fixed_lines.join("\n"));
    assert_eq!(fixed.stages[blaze_engine::Stage::Bodies.index()].entered, 4, "`one` and `h` entered after the fix, each under its library and its test build (D191: a row each unit that holds the body): {}", entries(&fixed));
    let (fixed_notes, clean) = &fixed.bodies;
    assert!(fixed_notes.is_empty(), "fixed: no body is refused: {fixed_notes:?}");
    assert_eq!(*clean, 2, "`one` and `h` checked clean after the fix");
    assert!(error_lines(&fixed_lines.join("\n")).is_empty(), "fixed: no error printed:\n{}", fixed_lines.join("\n"));
}

/// A LINT FINDING IS PRINTED AS `check` PRINTS IT: an unsafe operation outside an `unsafe`
/// block in a 2024 `unsafe fn` (the typed lint `unsafe-op-in-unsafe-fn`, advice at every step):
/// the engine lists what `check` lists and prints the same warning line, and the certificate
/// prints it again. The value used after its move makes no borrow finding on this shelf (its
/// `Copy` fact has no lang item: the borrow check is refused for both commands), so a borrow
/// finding goes through the same placed-finding path untested here.
#[test]
fn engine_check_prints_a_lint_and_a_borrow_finding_as_check_does() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(
        &io,
        "src/lib.rs",
        "pub struct S;\n\npub fn moved() -> u8 {\n    let s = S;\n    let _t = s;\n    let _u = s;\n    1\n}\n\npub unsafe fn read(p: *const u8) -> u8 {\n    *p\n}\n",
    );
    let (ids, lines, _) = said(&io, false);
    let engine_said = lines.join("\n");
    println!("engine-check listed {ids:?}\nengine-check printed:\n{engine_said}\n{}", closing(&io));
    // The old checker was this test's oracle until 2026-11-19 (it listed the lint once an
    // owner); the lint's place is the oracle now: one finding, once, at `*p`'s line.
    assert!(ids.iter().any(|(code, _)| code == "unsafe-op-in-unsafe-fn"), "the lint is listed: {ids:?}");
    assert_eq!(ids.iter().filter(|(code, _)| code == "unsafe-op-in-unsafe-fn").count(), 1, "the body checked once, its lint listed once: {ids:?}");
    let warnings = warning_lines(&engine_said);
    assert_eq!(warnings.len(), 1, "one warning line:\n{engine_said}");
    assert!(warnings[0].contains("src/lib.rs:11:") && warnings[0].contains("unsafe-op-in-unsafe-fn"), "the warning at `*p`:\n{engine_said}");
    assert!(error_lines(&engine_said).is_empty(), "no error line (no borrow finding on this shelf):\n{engine_said}");
    let (again_ids, again, certificate) = said(&io, false);
    assert!(certificate.certified, "nothing changed: the certificate; {}", entries(&certificate));
    // The closing line holds the time and the rebuilt count, which a certificate changes.
    let body = |lines: &[std::string::String]| lines[..lines.len() - 1].to_vec();
    assert_eq!((again_ids, body(&again)), (ids, body(&lines)), "the certificate prints the same");
}

/// A workspace of one member, `app`, under the `suggest` step, its module `a` holding `f`, which
/// calls itself (blaze-lint `recursion`), and `g`, which does not. `f`'s comment and `g`'s body
/// are the edits below, each keeping every length.
fn linted(io: &Io, comment: &str, g_body: &str) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n\n[workspace.metadata.blaze]\nlints = \"suggest\"\n");
    write(io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "app/src/lib.rs", "pub mod a;\n");
    write(io, "app/src/a.rs", &format!("pub fn f(n: u8) -> u8 {{\n    // {comment}\n    if n == 0 {{ 0 }} else {{ f(n - 1) }}\n}}\n\npub fn g(n: u8) -> u8 {{\n    {g_body}\n}}\n"));
}

/// The `Lints` stage's counts of one request: (entered, identical).
fn lint_counts(checked: &engine_check::Checked) -> (u32, u32) {
    let count = checked.stages[blaze_engine::Stage::Lints.index()];
    (count.entered, count.identical)
}

/// THE LINTS STAGE (engine_lints.rs, lints-stage.md): ONE task an item, every selected rule in
/// its one walk, keyed by the item's text.
/// (a) A token lint in `f` and none in `g`: `engine-check` prints the warning line `check` prints
///     (CONTROL: there is one, `recursion` at `f`'s name).
/// (b) An edit of `g`'s body (its length kept) enters `g`'s lint task alone, not `f`'s; `f`'s
///     finding stands and prints from the stored answer.
/// (c) A comment edit inside `f` (its length kept): the floor, 1 entered and 1 identical.
/// (d) Nothing changed: the certificate enters no lint task and prints the same lines.
/// (e) The profile's by-rule line names `recursion` with a nonzero cost and its one finding.
/// Prints `lints stage: a <warning>; b entered <units>; c <entered, identical>; e <line>`.
#[test]
fn engine_check_lints_each_item_by_its_text() {
    let io = Io::simulated(SimWorld::new(14, Limits::USUAL));
    linted(&io, "aa", "n");
    // (a) The one warning, at its place (the old checker's line was this step's second oracle
    // until 2026-11-19; the exact line below is the oracle now).
    let (_, first, checked) = said(&io, false);
    let first_said = first.join("\n");
    let warnings = warning_lines(&first_said);
    assert_eq!(warnings.len(), 1, "one warning, `f`'s recursion:\nengine-check said:\n{first_said}\n{}", entries(&checked));
    assert!(warnings[0].contains("app/src/a.rs:1:8: warning[recursion]: `f` calls itself (at `f`)"), "placed at `f`'s name, naming the call: {warnings:?}");
    assert!(checked.lints.entered.contains(&std::string::String::from("app/src/a.rs::f")) && checked.lints.entered.contains(&std::string::String::from("app/src/a.rs::g")), "every unit linted first: {:?}", checked.lints);
    assert_eq!(checked.lints.findings, 1, "one finding made: {:?}", checked.lints);
    // (e) The by-rule line: `recursion`, a nonzero cost, one finding.
    let text = profile(&io);
    let line = text.lines().find(|line| line.starts_with("lints: ") && line.contains("by rule:")).unwrap_or_else(|| panic!("the profile has the lints line:\n{text}")).to_owned();
    let recursion = line.split("by rule:").nth(1).and_then(|rules| rules.split(',').map(str::trim).find(|one| one.starts_with("recursion "))).unwrap_or_else(|| panic!("`recursion` is on the line: {line}"));
    let numbers: std::vec::Vec<u32> = recursion.split(' ').skip(1).map(|word| word.parse().expect("a count")).collect();
    assert!(numbers.len() == 2 && numbers[0] > 0 && numbers[1] == 1, "`recursion`: a nonzero cost and one finding: {line}");

    // (d) Nothing changed: the certificate, no lint task, the same lines.
    let (_, again, certificate) = said(&io, false);
    assert!(certificate.certified && lint_counts(&certificate) == (0, 0), "nothing changed: no lint task; {}", entries(&certificate));
    assert_eq!(warning_lines(&again.join("\n")), warnings, "the certificate prints the warning again");

    // (b) `g`'s body edited: `g`'s lint task alone; `f`'s finding from the stored answer.
    linted(&io, "aa", "7");
    let (_, edited, body) = said(&io, false);
    assert_eq!(body.lints.entered, [std::string::String::from("app/src/a.rs::g")], "only `g`'s lint task: {:?}\n{}", body.lints, entries(&body));
    assert_eq!(lint_counts(&body).0, 1, "one lint task entered: {}", entries(&body));
    assert_eq!(warning_lines(&edited.join("\n")), warnings, "`f`'s finding stands and prints from the stored answer");

    // (c) A comment inside `f`, every range kept: the floor, 1 entered and 1 identical.
    linted(&io, "bb", "7");
    let (_, commented, comment) = said(&io, false);
    assert_eq!(comment.lints.entered, [std::string::String::from("app/src/a.rs::f")], "only `f`'s lint task: {:?}", comment.lints);
    assert_eq!(lint_counts(&comment), (1, 1), "a comment: 1 entered, 1 identical; {}", entries(&comment));
    assert_eq!(warning_lines(&commented.join("\n")), warnings, "the same warning after a comment");
    std::println!("lints stage: a {}; b entered {:?}; c {:?}; e {line}", warnings[0], body.lints.entered, lint_counts(&comment));
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

/// The names reads of every body of the stored answer, judged against what each crate's
/// `CrateScope` row defines (lane ND): a cell the row defines, read at the digest it defines;
/// a cell of a crate with a cell table (`CELLS`) that the table does not hold, read ABSENT (a
/// name not found: absent by intent, engine_bodies.rs `widen`); or the record `WHOLE` at its
/// out. Anything else is a read no edit moves (rule 21) and is returned in the first list.
/// The counts: (cells defined, absent, whole of a crate with cells, whole of one without).
fn names_reads_of_bodies(io: &Io) -> (std::vec::Vec<std::string::String>, [u32; 4]) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let judged = crate::engine_why::with_kept(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), &mut phases, &err, |kept| {
        let rows = &kept.answer.rows;
        let (mut bad, mut counts) = (std::vec::Vec::new(), [0u32; 4]);
        for (name, row) in rows.iter() {
            if name.kind != blaze_drive::Kind::BodyCheck {
                continue;
            }
            for (id, digest) in row.reads.iter() {
                if id.of.kind != blaze_drive::Kind::CrateScope {
                    continue;
                }
                let Some(owner) = rows.get(&id.of) else {
                    bad.push(format!("{name:?} reads {id:?} of a crate with no row"));
                    continue;
                };
                let defined = |cell: blaze_drive::cells::Cell| owner.defines.iter().find(|(c, _)| *c == cell).map(|(_, d)| *d);
                let cells = defined(crate::engine_tasks::CELLS).is_some();
                match defined(id.cell) {
                    Some(d) if d == *digest && id.cell == crate::engine_tasks::WHOLE => counts[if cells { 2 } else { 3 }] += 1,
                    Some(d) if d == *digest => counts[0] += 1,
                    Some(d) => bad.push(format!("{name:?} reads {id:?} at {digest:?}, its crate defines {d:?}")),
                    None if cells && *digest == blaze_drive::key::ABSENT => counts[1] += 1,
                    None => bad.push(format!("{name:?} reads {id:?} at {digest:?}: no cell its crate defines (cell table: {cells})")),
                }
            }
        }
        (bad, counts)
    });
    match judged {
        Ok(Some(judged)) => judged,
        Ok(None) => panic!("no stored answer to judge the names reads of"),
        Err(error) => panic!("the stored answer: {error:?}"),
    }
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
    // Every names read of a body names a cell its crate's task defines, or is absent by intent.
    let (bad, counts) = names_reads_of_bodies(&io);
    println!("NAMES READS of the bodies: {} cells defined, {} absent, {} whole of a crate with cells, {} whole of one without", counts[0], counts[1], counts[2], counts[3]);
    assert!(bad.is_empty(), "names reads no cell its crate defines answers:\n{}", bad.join("\n"));
    assert!(counts[0] > 0, "no body read a defined names cell: {counts:?}");
    let bodies =|checked: &engine_check::Checked| -> std::vec::Vec<(&'static str, Option<blaze_drive::Kind>, u32)> {
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
    // A private fn added: the new fn's body, new, and no other (each body reads its heads by
    // their head tasks' cells, engine_heads.rs, lane EY; before, every body that read a head of
    // the file was entered by the file's `HEADS` cell: "Bodies read moved FileItems 8").
    let geom = "use crate::stats::*;\n\npub fn area(w: u32, h: u32) -> u32 {\n    scale(w) * h\n}\n\nfn scale(x: u32) -> u32 {\n    x + crate::stats::total(0, 0)\n}\n\npub fn side() -> u32 {\n    total(1, 1)\n}\n";
    // The new body's two rows (D191: its library's and its test build's), both new.
    let new_alone: &[(&'static str, Option<blaze_drive::Kind>, u32)] = &[("new", None, 2)];
    edit("a private fn added", "src/geom.rs", &format!("{geom}\nfn extra() -> u32 {{\n    7\n}}\n"), Some(0), 2, Some(new_alone), true);
    let stats = "pub fn total(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mean(a: u32, b: u32) -> u32 {\n    total(a, b) / 2\n}\n\npub fn spread() -> u32 {\n    crate::geom::side() + 1\n}\n";
    edit("a pub fn added nobody calls", "src/stats.rs", &format!("{stats}\npub fn unused() -> u32 {{\n    9\n}}\n"), Some(0), 2, Some(new_alone), true);
    // A `use` no body reads: the names move and no body is entered.
    let lib = "mod geom;\nmod stats;\n\npub fn top() -> u32 {\n    geom::area(2, 3) + stats::mean(4, 6)\n}\n\npub fn helper() -> u32 {\n    stats::total(1, 2)\n}\n";
    edit("a `use` no body reads", "src/lib.rs", &format!("use stats::spread;\n{lib}"), Some(0), 0, Some(&[]), true);
    // A `use` that shadows the glob's `total` in `geom`: `side`, which looks `total` up there
    // (its binding cell, absent before, moved), is entered; with `HEADS` moving too the cause
    // names whichever moved read comes first, so only "entered, not new" is asserted.
    let shadow = edit("a `use` added", "src/geom.rs", &format!("use crate::stats::mean as total;\n{geom}\nfn extra() -> u32 {{\n    7\n}}\n"), None, 0, None, true);
    assert!(bodies(&shadow).iter().any(|(cause, _, _)| *cause == "read moved"), "a `use` added: `side` entered: {}", entries(&shadow));
    // A pub fn renamed: the renamed body, new; its three callers (`mean`, `scale`, `helper`)
    // entered, by a names cell or by the file's `HEADS` (the first read that moved names the
    // cause), and they then fail.
    let renamed = stats.replace("pub fn total(", "pub fn sum(");
    let checked = edit("a pub fn renamed", "src/stats.rs", &format!("{renamed}\npub fn unused() -> u32 {{\n    9\n}}\n"), None, 2, None, false);
    let entered: u32 = bodies(&checked).iter().filter(|(cause, _, _)| *cause != "new").map(|(_, _, n)| *n).sum();
    assert!(entered >= 3, "a pub fn renamed: its three callers entered: {}", entries(&checked));
}

/// Three members in a chain: `c` reads `b`, `b` reads `a`.
pub(super) fn three_crates(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "pub mod m;\n\npub fn f() -> u8 {\n    1\n}\n");
    write(io, "a/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", "pub fn g() -> u8 {\n    a::f() + a::m::k()\n}\n");
    write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n");
    write(io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::g() + 1\n}\n");
}

/// Three members in a chain, `c` reads `b`, `b` reads `a`, and `a` has one import that names
/// nothing (`nowhere`, no crate). `reexport`: the import is `pub use` and `b` has one more body,
/// `bad`, that names `a::Missing` through it.
fn broken_chain(io: &Io, broken: bool, reexport: bool) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    let import = match (broken, reexport) {
        (false, _) => "",
        (true, false) => "use nowhere::Missing;\n\n",
        (true, true) => "pub use nowhere::Missing;\n\n",
    };
    write(io, "a/src/lib.rs", &format!("{import}pub fn good() -> u8 {{\n    1\n}}\n"));
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    let bad = if reexport { "\npub fn bad() -> u8 {\n    let _m: a::Missing;\n    2\n}\n" } else { "" };
    write(io, "b/src/lib.rs", &format!("pub fn f() -> u8 {{\n    a::good() + 1\n}}\n{bad}"));
    write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n");
    write(io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::f() + 1\n}\n");
}

/// One request's whole printed text (its closing line too) and its counts.
fn printed(io: &Io, fresh: bool) -> (std::vec::Vec<(std::string::String, std::string::String)>, std::string::String, engine_check::Checked) {
    let before = stdout(io).len();
    let (ids, checked) = engine_from(io, 2, fresh);
    (ids, stdout(io).split_off(before), checked)
}

/// The words a cascade printed: a task that waited on a failed one, a member refused whole.
const CASCADE: [&str; 3] = ["waits on a task that failed", "has no `Names` task", "has no names record"];

fn no_cascade(text: &str, when: &str) {
    for words in CASCADE {
        assert!(!text.contains(words), "{when}: printed {words:?}, a failure stopping more than what reads it:\n{text}");
    }
}

/// The `Names` stage's entries of one request.
fn names_entered(checked: &engine_check::Checked) -> u32 {
    checked.stages[blaze_engine::Stage::Names.index()].entered
}

/// AN ERROR STOPS ONLY WHAT READS IT (plan row A8): `a`'s one unresolved import is one error,
/// at it; `a` still writes its names record (the finding is in it), so `b` and `c` are solved
/// and every body is checked clean; nothing "waits on a task that failed" and no member is
/// refused whole. With nothing changed the certificate stands with the finding (no task
/// entered); fixed, the error is gone, `a`'s names were entered again, the answer is a fresh
/// run's, and the next request is the certificate again. CONTROL: the fixed tree prints no
/// error, so the one error is `a`'s import.
#[test]
fn an_unresolved_import_stops_only_what_reads_it() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    broken_chain(&io, true, false);
    let (_, first, checked) = printed(&io, false);
    println!("broken a:\n{first}\n{}", entries(&checked));
    no_cascade(&first, "broken a");
    let errors = error_lines(&first);
    assert_eq!(errors.len(), 1, "one error, at its cause:\n{first}");
    assert!(first.contains("nowhere"), "the error names the import's path:\n{first}");
    assert!(names_entered(&checked) >= 3, "every member's names task ran: {}", entries(&checked));

    // Nothing changed: the certificate stands with the finding.
    let (_, again, certificate) = printed(&io, false);
    assert!(certificate.certified && certificate.entered == 0, "nothing changed: no task entered; {}", entries(&certificate));
    assert_eq!(error_lines(&again), errors, "the certificate prints the error again");

    // Fixed: no error, `a`'s names entered, as a fresh run.
    broken_chain(&io, false, false);
    let (fixed_ids, fixed, fixed_checked) = printed(&io, false);
    println!("fixed a: {}", entries(&fixed_checked));
    assert!(error_lines(&fixed).is_empty(), "fixed: no error:\n{fixed}");
    // The `Names` entries, by cause, at the floor (2/2): `a`'s two units (its library and its
    // test unit) by its edited file; `b` and `c` read `a`'s `EXPORTS` cell, which does not move
    // when a private import is fixed, so 0 downstream `Names` tasks enter.
    let mut names_because: std::vec::Vec<(std::string::String, std::string::String, u32)> = fixed_checked
        .because
        .iter()
        .filter(|(group, _)| group.stage == blaze_engine::Stage::Names)
        .map(|(group, count)| (group.cause.to_owned(), format!("{:?}", group.definer.map(|name| name.kind)), *count))
        .collect();
    names_because.sort();
    let expected: std::vec::Vec<(std::string::String, std::string::String, u32)> =
        vec![("read moved".to_owned(), "Some(FileItems)".to_owned(), 2)];
    assert_eq!(names_because, expected, "fixed: `a`'s two units by its file alone (downstream reads EXPORTS): {}", entries(&fixed_checked));
    assert_eq!(fixed_checked.stages[blaze_engine::Stage::Names.index()], blaze_engine::StageCount { entered: 2, identical: 0 }, "fixed: only `a`'s two units entered: {}", entries(&fixed_checked));
    let (fresh_ids, fresh, _) = printed(&io, true);
    assert_eq!((placed_ids(&fixed_ids), error_lines(&fixed)), (placed_ids(&fresh_ids), error_lines(&fresh)), "fixed: the incremental answer is a fresh run's");
    let (_, _, settled) = printed(&io, false);
    assert!(settled.certified && settled.entered == 0, "fixed, then nothing changed: no task entered; {}", entries(&settled));
}

/// A PATH THROUGH THE BROKEN IMPORT FAILS ONLY ITS READER (plan row A8): `a`'s `pub use
/// nowhere::Missing;` is one error; `b::bad`, whose body names `a::Missing`, is the one other,
/// naming `Missing`; `b::f` and `c::h` check clean, and no member is refused whole.
#[test]
fn a_body_through_a_broken_import_alone_fails() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    broken_chain(&io, true, true);
    let (_, text, checked) = printed(&io, false);
    println!("a re-exports a broken import:\n{text}\n{}", entries(&checked));
    no_cascade(&text, "a broken re-export");
    let errors = error_lines(&text);
    assert_eq!(errors.len(), 2, "the import's error and `b::bad`'s, no other:\n{text}");
    assert!(errors.iter().any(|line| line.contains("Missing")) || text.contains("a::Missing"), "`b::bad`'s error names `a::Missing`:\n{text}");
    assert!(!text.contains("c/src/lib.rs"), "`c` reads nothing through the import: nothing of it is printed:\n{text}");
}

/// A crate whose names cannot be made at all (a member proc macro: the test shelf has no
/// `wasm32-wasip1` side, names/scope.rs `prepare`'s refused crate) and a member that depends on
/// it and names nothing of it.
pub(super) fn failed_dep(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"p\", \"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "p/Cargo.toml", "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n");
    write(io, "p/src/lib.rs", "pub fn nothing() {}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\np = { path = \"../p\" }\n");
    write(io, "b/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n");
    write(io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::f() + 1\n}\n");
}

/// A FAILED DEPENDENCY STOPS ONLY WHAT READS IT (plan row A8; tempo: two failed build scripts
/// left 110 names tasks "waiting" and 13,538 bodies refused): `p`'s `CrateScope` task fails,
/// once, naming itself; `b` and `c` are still solved (`b` against `p` as an error crate) and
/// nothing "waits on a task that failed"; their bodies' refusal names `p` (body-level isolation
/// is the next piece). The failed row records what it read, so with nothing
/// changed the certificate stands with the failure: NO task entered. CONTROL: the fixture's
/// dependency really failed (its failure is printed), so the test is of a failure.
#[test]
fn a_failed_dependency_leaves_its_dependents_solved() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    failed_dep(&io);
    let (_, text, checked) = printed(&io, false);
    println!("p fails:\n{text}\n{}", entries(&checked));
    assert!(text.contains("crate `p`"), "control: `p`'s names failed, naming it (else the fixture fails nothing):\n{text}");
    assert!(!text.contains("waits on a task that failed"), "a dependent of `p` waited on it:\n{text}");
    assert!(names_entered(&checked) >= 3, "`p`, `b` and `c` each entered their names task: {}", entries(&checked));
    // `b` and `c` have names records (only `p`'s two units fail, each once); their bodies are
    // still refused whole, since `p` is in their check scope and the bodies stage cuts the
    // scope at the first crate without records (engine_bodies.rs `scope_of`). Today's truth,
    // asserted: the refusal names `p`. Next lane (plan row A8, NOT DONE): a records hole per
    // failed crate in `group.crates`, so only a body that reads through `p` fails.
    assert_eq!(text.matches("a crate the toolchain shelf cannot build").count(), 2, "`p`'s library and test unit fail, nothing else:\n{text}");
    for member in ["b/src/lib.rs", "c/src/lib.rs"] {
        assert!(
            text.lines().any(|line| line.contains(member) && line.contains("check scope holds `p`")),
            "{member}'s refusal names `p`, the crate that failed:\n{text}"
        );
    }
    let (_, again, certificate) = printed(&io, false);
    assert!(certificate.entered == 0, "nothing changed: no task entered, the failed row stands by its reads; {}", entries(&certificate));
    // (The certificate standing WITH `p`'s failure is plan row D6's assertion: until "owed" is
    // exact and a certificate may hold findings, a kept finding means no certificate.)
    assert!(again.contains("crate `p`"), "and the failure is printed again:\n{again}");
}

/// THE RECORDS' COST (rules 50, 51; engine_store.rs `Ledger`): each stage's line, "N put, B
/// bytes; held after the merge: H bytes; copies made: C", on a three-crate chain, and the
/// digest of every record put, printed (`-- --show-output`) so the bytes on disk are seen to
/// stay the same as the way a record is written changes (rule 25).
/// The before (tree of piece 1): every record was joined into one buffer and copied again into
/// the store's arena, which kept it to the command's end, and each result kept its payload: two
/// copies a put, every byte held. Now the head and the payload go to the store as two slices (no
/// join), a names or scripts result frees its payload once it is put, and the phase's puts are
/// committed at once, the arena freeing every chunk no get lent from: one copy a put (the
/// arena's, until the commit) and nothing of a names record held after its merge. That the
/// arena's copy is seen is the control that the counter counts a copy; blaze-cas lent_tests
/// `a_commit_frees_every_put_no_get_lent` holds the arena's planted control.
#[test]
fn engine_check_counts_the_records_it_holds() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let (_, checked) = engine(&io, 2);
    for stage in blaze_engine::Stage::ALL {
        println!("records ({stage:?}): {}", checked.records.lines[stage.index()]);
    }
    println!("records put: digest {:?}", checked.records.digest());
    let names = checked.records.lines[blaze_engine::Stage::Names.index()];
    assert!(names.puts >= 3, "a names record a member at least: {names}");
    assert_eq!(names.copies, names.puts, "each names record copied once, into the arena, and joined nowhere: {names}");
    assert_eq!(names.results_held, 0, "nothing of a names record is held in the stage's results after its merge: {names}");
}

/// The digest of the answers of [`engine_check_reads_records_in_place`]: the bodies' notes,
/// their clean count and every record put. TODO-digest: the coordinator fills it from the first
/// run on the tree BEFORE the in-place reader (so the constant shows the answers did not move),
/// then from this tree; both must print the same. They did (b0b8b00a0dd0, 2026-11-17), and the
/// digest is PRINTED, not pinned, since: every later piece that adds a record moves it by design.
const READS_IN_PLACE_DIGEST: &str = "printed, not pinned";

/// THE READ SIDE HOLDS NOTHING (runtime.md C3; engine_records.rs): on the three-crate chain the
/// Heads and Bodies stages read every crate's `CrateScope` record in place from the store's
/// mapping, "records read (Bodies): N, B bytes; copied into the heap: 0 bytes", and answer as
/// before (the digest). The before: each crate's three sections were copied 16-aligned into the
/// heap once a command (body_types.rs `Kept::records`, front_load.rs `crate_records`), 670 MB on
/// tempo. CONTROL: front_load.rs `the_copying_route_counts_every_byte_it_copies` opens the same
/// records through the copying route and sees its copy counted, so a 0 here is a count, not a
/// counter that counts nothing.
#[test]
fn engine_check_reads_records_in_place() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    three_crates(&io);
    let (_, checked) = engine(&io, 2);
    for stage in blaze_engine::Stage::ALL {
        println!("records read ({stage:?}): {}", checked.records.reads[stage.index()]);
    }
    let heads = checked.records.reads[blaze_engine::Stage::Heads.index()];
    let bodies = checked.records.reads[blaze_engine::Stage::Bodies.index()];
    assert!(heads.records + bodies.records >= 3, "each member's records read at least once, in place: heads {heads}, bodies {bodies}");
    assert!(bodies.records + bodies.again > 0, "the Bodies stage read its crates' records: {bodies}");
    assert_eq!((heads.copied, bodies.copied), (0, 0), "no byte of a record read was copied into the heap: heads {heads}, bodies {bodies}");
    let mut answers = blaze_ids::Hasher::new("blaze-cli engine check reads in place test");
    for note in checked.bodies.0.iter() {
        answers.update(note.as_bytes());
    }
    answers.update(&checked.bodies.1.to_le_bytes()).update(&checked.records.digest().0);
    let digest = format!("{:?}", answers.finish());
    println!("READS_IN_PLACE_DIGEST {digest}");
    assert_ne!(digest, READS_IN_PLACE_DIGEST, "a digest of the answers is printed above");
}

// THE SWEEP COSTS THE CHANGES (plan row S1, journal_sweep.rs): on a world whose system keeps a
// journal of changes across a restart (FSEvents'), a repeat request replays it from the place
// the pointer saved and stats only the rows its names reach. Each test prints its `sweep:` line.

/// A world playing FSEvents' journal, from before its first write.
fn journaled(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    io.with_sim(|sim| sim.set_journal(blaze_core::sim::Journal::FsEvents)).expect("a simulated world");
    io
}

/// The profile's `sweep:` line.
fn sweep_line(io: &Io) -> std::string::String {
    profile(io).lines().find(|line| line.starts_with("sweep:")).map(std::string::String::from).unwrap_or_default()
}

/// (a) NOTHING CHANGED: the journal names nothing since the pointer's place, no row is statted,
/// and the certificate stands. CONTROL: the first request had no place saved and says so.
#[test]
fn a_repeat_with_nothing_changed_stats_no_path() {
    let io = journaled(21);
    typed_crates(&io, Typed::Plain);
    let (_, first) = engine_from(&io, 2, false);
    assert_eq!(first.sweep.journal, journal_sweep::JournalUse::Refused(journal_sweep::Refused::NoCursor), "the first request has no place: {}", sweep_line(&io));
    let (_, again) = engine_from(&io, 2, false);
    println!("nothing changed: {}", sweep_line(&io));
    assert!(again.certified, "nothing moved: the certificate");
    assert_eq!(again.sweep.journal, journal_sweep::JournalUse::Used, "{}", sweep_line(&io));
    assert_eq!((again.sweep.events, again.sweep.statted), (0, 0), "no event, no path statted: {}", sweep_line(&io));
    assert!(again.sweep.leaves > 0, "the leaf table holds the answer's reads");
    assert!(sweep_line(&io).starts_with("sweep: statted 0 of ") && sweep_line(&io).contains("(by events since id "), "{}", sweep_line(&io));
}

/// (b) ONE FILE WRITTEN: the journal names it (and the publish's temporary name beside it, which
/// no listing reads), its row alone is statted, and its outline alone is entered.
#[test]
fn a_repeat_after_one_write_stats_that_file_alone() {
    let io = journaled(22);
    typed_crates(&io, Typed::Plain);
    write(&io, "a/src/lib.rs", "pub mod m;\n\npub fn one() -> u8 {\n    m::two()\n}\n");
    write(&io, "a/src/m.rs", "pub fn two() -> u8 {\n    2\n}\n");
    let _ = engine_from(&io, 2, false);
    write(&io, "a/src/m.rs", "pub fn two() -> u8 {\n    2\n}\n// a comment\n");
    let (_, now) = engine_from(&io, 2, false);
    println!("one write: {}", sweep_line(&io));
    assert_eq!(now.sweep.journal, journal_sweep::JournalUse::Used, "{}", sweep_line(&io));
    assert!(now.sweep.events >= 1, "the write is named: {}", sweep_line(&io));
    assert_eq!(now.sweep.statted, 1, "the written file's row alone: {}", sweep_line(&io));
    assert!(!now.certified, "the file moved");
    assert_eq!(now.stages[blaze_engine::Stage::Outlines.index()].entered, 1, "its outline alone entered\n{}", entries(&now));
}

/// (c) THE JOURNAL WAS RESET (another volume UUID): the stated full sweep, said with its reason,
/// and the answer is a fresh run's.
#[test]
fn a_reset_journal_sweeps_all_and_says_why() {
    let io = journaled(23);
    typed_crates(&io, Typed::Wrong);
    let _ = engine_from(&io, 2, false);
    io.with_sim(|sim| sim.reset_journal()).expect("a simulated world");
    write(&io, "b/src/lib.rs", "pub fn h() -> u8 {\n    a::one() + 2\n}\n\npub fn wrong() -> u8 {\n    true\n}\n");
    let (ids, now) = engine_from(&io, 2, false);
    let line = sweep_line(&io);
    println!("reset: {line}");
    assert_eq!(now.sweep.journal, journal_sweep::JournalUse::Refused(journal_sweep::Refused::Lost(bstd::fs::Lost::Reset)), "{line}");
    assert_eq!(now.sweep.statted, now.sweep.leaves, "every row statted: {line}");
    assert!(line.contains("refused: the journal of changes was reset"), "the reason is printed: {line}");
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    assert_eq!((ids, now.status.0, now.bodies.0.clone()), (fresh_ids, fresh.status.0, fresh.bodies.0.clone()), "the answer is a fresh run's");
}

/// (d) ONE FOLDER TO SCAN AGAIN (FSEvents' `MustScanSubDirs` on `b`): the rows beneath it (and
/// the listing it is in) are statted, none of `a`'s. CONTROL: the whole checkout lost stats
/// every row.
#[test]
fn a_lost_subtree_stats_that_subtree_alone() {
    let io = journaled(24);
    typed_crates(&io, Typed::Plain);
    let _ = engine_from(&io, 2, false);
    let b = blaze_core::FileName::new("ws/b").expect("a name");
    io.with_sim(|sim| sim.lose_changes(&b, bstd::fs::Lost::Rescan)).expect("a simulated world");
    let (_, now) = engine_from(&io, 2, false);
    println!("b to scan again: {}", sweep_line(&io));
    assert_eq!(now.sweep.journal, journal_sweep::JournalUse::Used, "{}", sweep_line(&io));
    assert!(now.certified, "nothing in b moved: the certificate");
    assert!(now.sweep.statted >= 3, "b's manifest, root and listing at least: {}", sweep_line(&io));
    assert!(now.sweep.statted < now.sweep.leaves, "a's rows are not statted: {}", sweep_line(&io));
    let ws = blaze_core::FileName::new("ws").expect("a name");
    io.with_sim(|sim| sim.lose_changes(&ws, bstd::fs::Lost::Rescan)).expect("a simulated world");
    let (_, all) = engine_from(&io, 2, false);
    println!("ws to scan again: {}", sweep_line(&io));
    // Every row of the checkout: all but the home's listings, whose journal lost nothing.
    assert!(all.sweep.statted > now.sweep.statted && all.sweep.statted + 2 >= all.sweep.leaves, "the checkout lost: every row of it statted: {}", sweep_line(&io));
}

/// (e) THE PLACE IS TAKEN BEFORE THE READS: a file written after a request's replay (and before
/// it would read the file) is named by the next replay from the place that request saves. The
/// save's bytes are read back as the next request reads them. CONTROL: with nothing written
/// between, the replay names nothing.
#[test]
fn a_write_after_the_replay_is_seen_by_the_next() {
    let io = journaled(25);
    typed_crates(&io, Typed::Plain);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let fs = io.need_fs().expect("files");
    let root = fs.open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let home = fs.open_dir(&cx, &Path::new("h/.blaze").expect("a path")).expect("open");
    let clock = io.need_clock().expect("a clock");
    let saved = |taken: &journal_sweep::Taken<'_>| {
        let mut bytes = blaze_own::Vec::new_in(frame);
        taken.encode(&mut bytes);
        bytes
    };
    let first = journal_sweep::take(&cx, &root, &home, &clock, journal_sweep::Stored::NONE, frame).expect("the replay");
    let first_bytes = saved(&first);
    // CONTROL: nothing written since the place.
    let stored = journal_sweep::Stored::of(&first_bytes).expect("the places read back whole");
    let quiet = journal_sweep::take(&cx, &root, &home, &clock, stored, frame).expect("the replay");
    assert_eq!((quiet.journal.clone(), quiet.events), (journal_sweep::JournalUse::Used, 0), "nothing written: nothing named");
    // The write a request's reads would race: after the place, before the read.
    write(&io, "a/src/lib.rs", "pub fn one() -> u8 {\n    1\n}\n// late\n");
    let next = journal_sweep::take(&cx, &root, &home, &clock, stored, frame).expect("the replay");
    println!("a write after the place: {} event(s)", next.events);
    assert_eq!(next.journal, journal_sweep::JournalUse::Used);
    assert!(next.events >= 1, "the late write is named by the next replay");
}

/// (f) A DEPENDENCY'S SOURCE IS NEVER STATTED: a file written under the home's registry sources
/// (a settled class, keyed by the lock) moves no row and is not counted: the certificate stands
/// with no path statted, and no settled class is checked.
#[test]
fn a_dependency_source_is_never_statted() {
    let io = journaled(26);
    typed_crates(&io, Typed::Plain);
    let _ = engine_from(&io, 2, false);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let source = Path::new(&format!("h/.blaze/v2/sources/{}/src/lib.rs", "0".repeat(64))).expect("a path");
    io.need_fs().expect("files").root().publish(&cx, &source, Content::bytes(b"pub fn dep() {}\n"), Durable::No).expect("write");
    let (_, now) = engine_from(&io, 2, false);
    println!("a dependency's source written: {}", sweep_line(&io));
    assert!(now.certified, "a dependency's source moves no row of the checkout");
    assert_eq!((now.sweep.events, now.sweep.statted, now.sweep.settled), (0, 0, 0), "{}", sweep_line(&io));
}

/// The stored answer's unit graph row: its out, the digest of the whole sealed payload (the
/// graph's bytes and the crate list), so two equal outs are two equal records.
fn stored_graph_out(io: &Io) -> blaze_ids::ContentId {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let got = crate::engine_why::with_kept(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), &mut phases, &err, |kept| kept.answer.rows.get(&blaze_drive::Name::unit_graph()).map(|row| row.out));
    got.expect("the stored answer opens").expect("an answer opens").expect("the answer has a unit graph row")
}

/// The tasks entered past the units stage: the graph's readers and theirs.
fn entered_past_units(checked: &engine_check::Checked) -> u32 {
    blaze_engine::Stage::ALL.iter().filter(|stage| **stage != blaze_engine::Stage::Units).map(|stage| checked.stages[stage.index()].entered).sum()
}

/// THE UNIT GRAPH IS MADE ONCE AND READ BACK (plan row D2): on three crates in a chain the first
/// request discovers it (no stored answer), a body edit reads it back (no manifest, lock,
/// toolchain or config moved: no fetch run, no graph walk), and a request with nothing moved is
/// the certificate, which does not open the units stage. The profile's plan line says which.
/// CONTROL: before D2 every request discovered the graph, and the plan line named no cause.
#[test]
fn engine_check_reads_the_unit_graph_back() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let (_, first) = engine_from(&io, 2, false);
    println!("first: units {:?}\n{}", first.units, profile_lines(&io));
    assert_eq!((first.units.read_back, first.units.discovered), (0, 1), "the first request discovers the graph: {:?}", first.units);
    assert!(profile(&io).contains("plan (the units, the fetches and the graph)"), "the first request's plan line says the graph was discovered:\n{}", profile_lines(&io));
    write(&io, "a/src/m.rs", "pub fn k() -> u8 {\n    let _ = 0u8;\n    2\n}\n");
    let (body_ids, body) = engine_from(&io, 2, false);
    println!("a body edit: units {:?}\n{}", body.units, profile_lines(&io));
    assert_eq!(body.units, crate::engine_units::Units { read_back: 1, discovered: 0, fetches_run: 0 }, "a body edit reads the graph back\n{}", profile_lines(&io));
    assert!(profile(&io).contains("plan (the units read back"), "the plan line says the graph was read back:\n{}", profile_lines(&io));
    // The read-back answer is a fresh run's.
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    assert_eq!((body_ids, body.status.0, body.bodies.0.clone()), (fresh_ids, fresh.status.0, fresh.bodies.0.clone()), "the answer over the graph read back is a fresh run's");
    let (_, again) = engine_from(&io, 2, false);
    assert!(again.certified, "nothing moved: the certificate");
    assert_eq!(again.units, crate::engine_units::Units::default(), "the certificate opens no units stage");
}

/// A REACHABLE MODULE IS WITHDRAWN FROM THE STANDING PLAN (plan row D2): deleting `a`'s module
/// and removing its `mod` edge changes the file plan while the manifests, lock and graph inputs
/// remain fixed. The graph is read back without a fetch or graph walk, and the resulting answer
/// equals a fresh request with the old module's body absent.
#[test]
fn engine_check_withdraws_a_deleted_reachable_module_from_the_plan() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let _ = engine_from(&io, 2, false);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    root.remove(&cx, &Path::new("a/src/m.rs").expect("a path"), Durable::No).expect("remove the module");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "b/src/lib.rs", "pub fn g() -> u8 {\n    a::f()\n}\n");
    let (ids, now) = engine_from(&io, 2, false);
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    println!("deleted reachable module: units {:?}, files {:?}, entries {}\n{}", now.units, now.files, now.bodies.1, profile_lines(&io));
    assert_eq!(now.units, crate::engine_units::Units { read_back: 1, discovered: 0, fetches_run: 0 }, "the unchanged graph is read back after a source deletion\n{}", profile_lines(&io));
    assert_eq!((ids, now.status.0, now.bodies.0.clone()), (fresh_ids, fresh.status.0, fresh.bodies.0.clone()), "withdrawing the deleted module matches a fresh request");
}

/// EARLY CUTOFF OF THE GRAPH (plan row D2): a comment in a member's `Cargo.toml` moves the
/// manifests, so the graph is discovered again, but it is the same graph: its record's bytes
/// equal the stored ones (its out, the digest of the sealed payload), and no task past the units
/// stage is entered. CONTROL: a dependency added in `c/Cargo.toml` writes another graph and
/// enters `c`'s names (engine_check_adds_a_dependency_to_the_graph).
#[test]
fn engine_check_cuts_off_a_manifest_comment() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let _ = engine_from(&io, 2, false);
    let before = stored_graph_out(&io);
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n# a comment\n\n[package.metadata.docs]\nall = true\n\n[dependencies]\na = { path = \"../a\" }\n");
    let (ids, now) = engine_from(&io, 2, false);
    println!("a manifest comment: units {:?}, entered {}\n{}", now.units, entries(&now), profile_lines(&io));
    assert_eq!((now.units.read_back, now.units.discovered), (0, 1), "a manifest moved: the graph discovered again");
    assert_eq!(stored_graph_out(&io), before, "the same graph: its record's bytes are the stored ones");
    assert_eq!(entered_past_units(&now), 0, "no task past the units stage entered\n{}", entries(&now));
    assert!(ids.is_empty(), "the comment adds no finding: {ids:?}");
}

/// A DEPENDENCY ADDED (plan row D2, the control of the cutoff): `c` now also depends on `a`
/// directly. The graph is discovered again, its record differs, and the names stage is entered
/// (`c`'s scope now holds `a`); the answer is a fresh run's.
#[test]
fn engine_check_adds_a_dependency_to_the_graph() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let _ = engine_from(&io, 2, false);
    let before = stored_graph_out(&io);
    write(&io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\n");
    write(&io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::g() + a::f()\n}\n");
    let (ids, now) = engine_from(&io, 2, false);
    println!("a dependency added: units {:?}, entered {}\n{}", now.units, entries(&now), profile_lines(&io));
    assert_eq!((now.units.read_back, now.units.discovered), (0, 1), "a manifest moved: the graph discovered again");
    assert!(stored_graph_out(&io) != before, "another graph: `c` depends on `a`");
    assert!(now.stages[blaze_engine::Stage::Names.index()].entered >= 1, "`c`'s names entered\n{}", entries(&now));
    let (fresh_ids, fresh) = engine_from(&io, 2, true);
    assert_eq!((ids, now.status.0, now.bodies.0.clone()), (fresh_ids, fresh.status.0, fresh.bodies.0.clone()), "the answer is a fresh run's");
}

/// `blaze why` READS A FILE UNDER ITS CRATE'S EDITION (plan row D2; engine_why.rs once looked
/// every item up under 2024): `a` is a 2021 crate, and `why` of its body names edition 2021,
/// from the stored unit graph; after `a/Cargo.toml` moves to 2024 it names 2024. CONTROL: with
/// the edition a constant, both said nothing of it, and a 2021-only parse was missed.
#[test]
fn engine_why_reads_a_body_under_its_crates_edition() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let a = |edition: &str| format!("[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n");
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", &a("2021"));
    write(&io, "a/src/lib.rs", "pub fn one() -> u8 {\n    let gen = 1u8;\n    gen\n}\n");
    let _ = engine_from(&io, 2, false);
    let said = why(&io, "a/src/lib.rs::one");
    println!("why under 2021:\n{said}");
    assert!(said.contains("the body of `a/src/lib.rs::one` (read as edition 2021"), "why names the crate's edition, 2021: {said}");
    write(&io, "a/Cargo.toml", &a("2024"));
    // `gen` is a keyword in 2024: the body written without it, so its item stands either way.
    write(&io, "a/src/lib.rs", "pub fn one() -> u8 {\n    let x = 1u8;\n    x\n}\n");
    let _ = engine_from(&io, 2, false);
    let said = why(&io, "a/src/lib.rs::one");
    println!("why under 2024:\n{said}");
    assert!(said.contains("(read as edition 2024"), "why names the crate's edition, 2024: {said}");
}
