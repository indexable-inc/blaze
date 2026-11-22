//! A member build script that asks git, end to end through `engine-check` on a simulated world.
//! The script is a test double (engine_scripts.rs `ScriptRunner`) in place of the job and the wasm
//! run: it asks git one question in `pkg`, records it as the sandbox records it (the subject
//! `git:pkg\n<words>`, today's answer by `engine_script_reads::now`), and prints
//! `cargo::rustc-cfg=pkg_flag`. Its record is made by the one maker of a script's record
//! (scripts.rs `outputs_record`), its reads through the one parser (`cells_of`). `pkg`'s `f`
//! exists under that cfg; `app`'s `g` calls it, so `app`'s call resolves only when `pkg`'s names
//! record is made with the script's outputs.
//!
//! - A git observation without an argument vector has no cell: the record is used this command and not kept
//!   (scripts.rs `PooledScript::unkept`), and the script runs again next command (owed).
//! - `rev-parse HEAD` has a cell (engine_script_reads.rs `git_now`): the record is kept and
//!   stands until the answer moves.
//! - THE CONTROL (rule 23): the old route, the run a hole (`Did::Hole`), its record dropped: the
//!   crates above it are refused "has no names record", as in the tempo run of tag g004958.

use super::*;

use core::sync::atomic::{AtomicU32, Ordering};

use crate::engine_script_reads::{self as reads, ReadKind, ReadsPlant};
use crate::sweep::{Plant, Stamp, Stamps};

use super::engine_check_tests::engine_from;

/// No stamp proves anything (the double reads every answer, as the sandbox does).
struct NoStamps;

impl Stamps for NoStamps {
    fn stamp(&self, _: &str, _: &bstd::fs::Meta) -> Option<Stamp> {
        None
    }

    fn swept_at_ns(&self) -> u64 {
        0
    }

    fn granularity_ns(&self) -> u64 {
        1
    }
}

/// The questions, as a `git:` subject's rest (script_sandbox.rs `git_subject`).
const HEAD_ASKED: &str = "pkg\nrev-parse HEAD";
const HEAD_SUBJECT: &str = "git:pkg\nrev-parse HEAD";
const UNNAMED_SUBJECT: &str = "git:pkg";

const COMMIT_ONE: &str = "1111111111111111111111111111111111111111\n";
const COMMIT_TWO: &str = "2222222222222222222222222222222222222222\n";

#[derive(Clone, Copy, Debug)]
enum Route {
    /// `rev-parse HEAD`: a cell; the record is kept.
    Cell,
    /// A tracked-only porcelain query in a checkout whose contents the test changes.
    Status(&'static AtomicU32),
    /// A git read with no command to replay.
    Unkept,
    /// THE CONTROL: the old route, the run a hole and its record dropped.
    OldHole,
}

struct Double {
    runs: AtomicU32,
    route: Route,
}

/// The cells of no read: what a run that hands none back carries.
fn no_reads(plant: ReadsPlant) -> reads::ReadList {
    reads::cells_of(core::iter::empty(), plant).expect("no read has a cell")
}

impl crate::engine_scripts::ScriptRunner for Double {
    fn checkout_tree(&self) -> Option<blaze_proc_macro::script_sandbox::Tree> {
        let Route::Status(state) = self.route else { return None };
        let mut files = super::engine_script_reads_tests::status_files(false);
        let bytes: &[u8] = match state.load(Ordering::SeqCst) { 0 => b"a\n", 1 => b"b\n", _ => b"c\n" };
        files.iter_mut().find(|(name, _)| name == "f").unwrap().1 = bytes.to_vec();
        Some(blaze_proc_macro::script_sandbox::mem_tree(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())), "", None))
    }

    fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<crate::engine_scripts::KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> crate::scripts::PooledScript {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let frame = cx.frame();
        let (plant, view) = kept.expect("the request plans the reads' cells, so a member's reads are kept");
        let done = |did: crate::scripts::Ran, checkout, unkept| crate::scripts::PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept };
        if matches!(self.route, Route::OldHole) {
            let failed = crate::scripts::failed("a member's build script made a read no cell names", format_args!("the build script of pkg 0.1.0 is run again next command, its outputs not kept (the old route, planted)"), true);
            return done(failed, no_reads(plant), None);
        }
        let recorded = match self.checkout_tree() {
            Some(tree) => ["git-scope:pkg", "git:\nstatus --porcelain --untracked-files=no"].into_iter()
                .map(|subject| (subject, blaze_proc_macro::script_sandbox::answer_now(&tree, subject).unwrap())).collect(),
            None => {
                // The broken recorder omits the argument vector from a real HEAD read.
                let (_, _, answer) = reads::now(cx, root, &NoStamps, Plant::None, ReadKind::Git, HEAD_ASKED, None, frame).unwrap();
                let subject = if matches!(self.route, Route::Cell) { HEAD_SUBJECT } else { UNNAMED_SUBJECT };
                vec![(subject, Some(answer))]
            }
        };
        let mut checkout = no_reads(plant);
        let unkept = reads::collect_reads(recorded.into_iter(), plant, view, &mut checkout).err();
        if unkept.is_some() {
            checkout.clear();
        }
        let mut cells: blaze_own::Vec<(blaze_drive::Name, blaze_ids::ContentId), &bstd::mem::Frame> = blaze_own::Vec::with_capacity_in(checkout.len(), frame);
        for read in checkout.iter() {
            cells.push((read.name(), read.answer));
        }
        let erased = blaze_own::Erased::new(frame);
        let outputs = blaze_build_script::outputs::Outputs::read(b"cargo::rustc-cfg=pkg_flag\n", "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle()).expect("the double's output reads");
        match crate::scripts::outputs_record(name, &cells[..], crate::scripts::RanAs::Wasm, &outputs, "pkg 0.1.0") {
            made @ crate::scripts::Ran::Made(_) => done(made, checkout, unkept),
            failed => done(failed, no_reads(plant), None),
        }
    }
}

fn world(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"pkg\", \"app\"]\nresolver = \"2\"\n");
    write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
    write(&io, "pkg/build.rs", "fn main() {\n    println!(\"cargo::rustc-cfg=pkg_flag\");\n}\n");
    write(&io, "pkg/src/lib.rs", "#[cfg(pkg_flag)]\npub fn f() -> u8 {\n    1\n}\n");
    write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\npkg = { path = \"../pkg\" }\n");
    write(&io, "app/src/lib.rs", "pub fn g() -> u8 {\n    pkg::f()\n}\n");
    write(&io, ".git/HEAD", "ref: refs/heads/main\n");
    write(&io, ".git/refs/heads/main", COMMIT_ONE);
    io
}

type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

/// One `engine-check` request from the stored answer, with `runner` for the scripts.
fn request(io: &Io, runner: &'static Double) -> (Ids, engine_check::Checked, std::string::String) {
    let before = stdout(io).len();
    let (ids, checked) = crate::engine_scripts::with_runner(runner, || engine_from(io, 1, false));
    (ids, checked, stdout(io).split_off(before))
}

fn runs(runner: &Double) -> u32 {
    runner.runs.load(Ordering::SeqCst)
}

#[test]
fn a_status_script_certifies_until_its_porcelain_bytes_change() {
    static STATE: AtomicU32 = AtomicU32::new(0);
    static RUNNER: Double = Double { runs: AtomicU32::new(0), route: Route::Status(&STATE) };
    let io = world(45);
    for (state, certified, expected_runs) in [(0, false, 1), (0, true, 1), (1, false, 2), (2, true, 2), (0, false, 3)] {
        STATE.store(state, Ordering::SeqCst);
        let (ids, checked, said) = request(&io, &RUNNER);
        assert!(ids.is_empty(), "{said}");
        assert_eq!((checked.certified, runs(&RUNNER)), (certified, expected_runs), "{said}");
        if certified {
            assert_eq!(checked.entered, 0);
        }
    }
}

/// Whether a group of the scripts stage was entered for `cause` (engine.rs `Cause::text`), and
/// for a read moved, because the cell `of` moved.
fn entered_for(checked: &engine_check::Checked, cause: &str, of: Option<blaze_drive::Name>) -> bool {
    checked.because.iter().any(|(group, _)| group.stage == blaze_engine::Stage::Scripts && group.cause == cause && (of.is_none() || group.definer == of))
}

/// What the note of an unkept run says (engine_scripts.rs `UnkeptWords`).
const UNKEPT: &str = "used by this command and not kept";

/// (a) a script whose recorded git subject lost its command: its record is used this command, so `pkg`'s names record is
/// made with its cfg and `app`'s body is checked, nothing waits on it and nothing is refused for
/// a missing names record; the note is said once. (b) the next command runs it again (owed),
/// with nothing changed, and is not certified.
#[test]
fn a_script_with_a_read_no_cell_names_is_used_this_command_and_runs_again_next() {
    static RUNNER: Double = Double { runs: AtomicU32::new(0), route: Route::Unkept };
    let io = world(41);
    let (ids, checked, said) = request(&io, &RUNNER);
    assert_eq!(runs(&RUNNER), 1, "the script ran once\n{said}");
    assert!(!said.contains("has no names record"), "no crate lacks its names record: {said}");
    assert!(!said.contains("task(s) not run"), "no task waits on the script: {said}");
    assert_eq!(said.matches(UNKEPT).count(), 1, "the note is said once: {said}");
    let profile = super::engine_check_tests::profile(&io);
    let rejected = profile.lines().find(|line| line.contains("Scripts: unkept package=pkg 0.1.0 unit=")).expect("the rejected package and unit have a profile row");
    assert!(rejected.contains("subject=\"git:pkg\" NoCell=Git"), "{rejected}");
    assert!(ids.is_empty(), "`app`'s call resolves under the script's cfg: {ids:?}\n{said}");
    assert!(checked.bodies.1 >= 1, "`app`'s body is checked: {:?}", checked.bodies.0);
    let (ids, again, said) = request(&io, &RUNNER);
    assert_eq!(runs(&RUNNER), 2, "nothing moved, and the script runs again: its row is owed\n{said}");
    assert!(entered_for(&again, "owed", None), "entered because owed: {:?}", again.because.iter().map(|(group, count)| (group.stage, group.cause, *count)).collect::<std::vec::Vec<_>>());
    assert!(!again.certified, "no certificate stands on a record kept for one command\n{said}");
    assert!(ids.is_empty(), "the answer is the same: {ids:?}\n{said}");
    assert_eq!(said.matches(UNKEPT).count(), 1, "said once again, for the run of this command: {said}");
}

/// (c) a script that asks `rev-parse HEAD`: the git cell keeps its record; the next command is
/// certified and runs nothing; a file of `.git` that the answer does not read moves nothing; a
/// new commit on the branch HEAD names moves the cell and runs the script again, named by it.
#[test]
fn a_git_cell_keeps_the_run_until_the_answer_moves() {
    static RUNNER: Double = Double { runs: AtomicU32::new(0), route: Route::Cell };
    let io = world(42);
    let (ids, _, said) = request(&io, &RUNNER);
    assert_eq!(runs(&RUNNER), 1, "the script ran once\n{said}");
    assert!(!said.contains(UNKEPT), "the git read has a cell: {said}");
    assert!(ids.is_empty(), "`app`'s call resolves: {ids:?}\n{said}");
    let (_, again, said) = request(&io, &RUNNER);
    assert!(again.certified, "nothing moved: the certificate stands\n{said}");
    assert_eq!(runs(&RUNNER), 1, "the script is not run again");
    write(&io, ".git/description", "a file no answer reads\n");
    let (_, again, said) = request(&io, &RUNNER);
    assert!(again.certified, "the answer did not move: certified\n{said}");
    assert_eq!(runs(&RUNNER), 1, "the script is not run");
    write(&io, ".git/refs/heads/main", COMMIT_TWO);
    let (_, checked, said) = request(&io, &RUNNER);
    assert_eq!(runs(&RUNNER), 2, "a new commit moves the answer: the script runs again\n{said}");
    assert!(
        entered_for(&checked, "read moved", Some(reads::cell_name(ReadKind::Git, HEAD_ASKED))),
        "the cause names the git cell: {:?}",
        checked.because.iter().map(|(group, count)| (group.stage, group.cause, group.definer, *count)).collect::<std::vec::Vec<_>>()
    );
}

/// THE CONTROL (rule 23): the same world with the old route planted (the run a hole, its record
/// dropped): the crates above it have no names record and are refused, as in the tempo run; the
/// unkept route of the first test, the cause removed, refuses none.
#[test]
fn control_the_old_hole_route_refuses_the_crates_above_it() {
    static PLANTED: Double = Double { runs: AtomicU32::new(0), route: Route::OldHole };
    static FIXED: Double = Double { runs: AtomicU32::new(0), route: Route::Unkept };
    let (_, _, planted) = request(&world(43), &PLANTED);
    assert!(planted.contains("has no names record"), "the old route drops the record: {planted}");
    let (ids, _, fixed) = request(&world(43), &FIXED);
    assert!(!fixed.contains("has no names record") && ids.is_empty(), "the record used this command refuses none: {ids:?}\n{fixed}");
}

/// The git cell's answer is the sandbox's, for each question a member's script asks and each
/// way HEAD names its commit: a loose ref, a packed ref, a detached head (rule 21: one cell, one
/// digest whoever makes it; script_sandbox.rs `answer_now` over the same files).
#[test]
fn a_git_cell_s_answer_is_the_sandbox_s() {
    let packed = "# pack-refs with: peeled fully-peeled sorted \n1111111111111111111111111111111111111111 refs/heads/main\n";
    let cases: [(&str, [(&str, &str); 2]); 3] = [
        ("a loose ref", [(".git/HEAD", "ref: refs/heads/main\n"), (".git/refs/heads/main", COMMIT_ONE)]),
        ("a packed ref", [(".git/HEAD", "ref: refs/heads/main\n"), (".git/packed-refs", packed)]),
        ("a detached head", [(".git/HEAD", COMMIT_TWO), (".git/description", "x\n")]),
    ];
    let questions = ["--version", "rev-parse --is-inside-work-tree", "rev-parse HEAD", "rev-parse --abbrev-ref HEAD", "branch --show-current", "symbolic-ref HEAD"];
    for (case, files) in cases.iter() {
        let io = Io::simulated(SimWorld::new(44, Limits::USUAL));
        write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n");
        for (path, text) in files.iter() {
            write(&io, path, text);
        }
        let manifest: (&str, &[u8]) = ("pkg/Cargo.toml", b"[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n");
        let tree = blaze_proc_macro::script_sandbox::mem_tree(files.iter().map(|(path, text)| (*path, text.as_bytes())).chain(core::iter::once(manifest)), "", None);
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        let frame = cx.frame();
        for words in questions.iter() {
            let mut asked = blaze_own::String::new_in(frame);
            asked.push_str("pkg\n");
            asked.push_str(words);
            let mut subject = blaze_own::String::new_in(frame);
            subject.push_str("git:");
            subject.push_str(asked.as_str());
            let (_, _, ours) = reads::now(&cx, &root, &NoStamps, Plant::None, ReadKind::Git, asked.as_str(), None, frame).expect("answered");
            let theirs = blaze_proc_macro::script_sandbox::answer_now(&tree, subject.as_str()).expect("a known subject").unwrap_or(reads::NONE);
            assert_eq!(ours, theirs, "{case}: `git {words}`");
            assert_ne!(ours, reads::NONE, "{case}: `git {words}` is answered, not refused");
        }
    }
}
