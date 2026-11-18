//! Two worktrees of one repository on one machine (remote-read.md, "Shared across worktrees
//! (2026-11-16)"): two folders with the same files at different paths, one home. Every drive
//! record is keyed by text alone (rules 25 and 71), so the second worktree's cold `check` finds
//! every record the first made in the one store under the home and runs no task; its findings equal a
//! fresh home's. An edit in one worktree reruns what it touches there, and the other's answer
//! stands. The planted bug lets the checkout's path into the keys: the second worktree then
//! rebuilds, and the first test's oracle says so.

use std::fmt::Write as _;

use blaze_drive::{Command, Tool};
use blaze_report::frame::plain::View;
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use crate::phases::Phases;
use crate::{close, graph, home, print, program, request, toolchain, words, work};

/// The user's home in the simulation, as `main` would be given `HOME=/h`, started in `/`.
const HOME: &str = "/h";
/// The first worktree and the second: siblings, as `git worktree add ../two` makes them.
pub(super) const ONE: &str = "one";
pub(super) const TWO: &str = "two";

const MANIFEST: &str = "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
/// A crate root with two modules and one unresolved import: an error the answer must carry.
const LIB: &str = "mod a;\nmod b;\npub fn f() -> u8 {\n    a::g()\n}\nuse crate::nope::Thing;\n";
const A: &str = "pub fn g() -> u8 { 1 }\n";
const B: &str = "pub fn h() -> u8 { 2 }\n";
/// `B` with one body edited: the edit the second worktree makes.
const B_EDITED: &str = "pub fn h() -> u8 { 3 }\n";

/// What one `check` in a worktree said and did.
pub(super) struct Ran {
    pub(super) runs: u32,
    pub(super) certified: bool,
    pub(super) errors: u32,
    pub(super) status: Option<u8>,
    /// Records the store answered, and records this check put (the profile's `hits`, `puts`).
    pub(super) found: u32,
    pub(super) written: u32,
    pub(super) profile: std::string::String,
    /// The finding lines it printed (the rest of standard output holds times).
    pub(super) findings: std::vec::Vec<std::string::String>,
}

fn write(io: &Io, at: &str, path: &str, text: &str) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let path = Path::new(&format!("{at}/{path}")).expect("a path");
    io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(text.as_bytes()), Durable::No).expect("write");
}

/// The same files in a worktree at `at`.
pub(super) fn worktree(io: &Io, at: &str) {
    write(io, at, "Cargo.toml", MANIFEST);
    write(io, at, "src/lib.rs", LIB);
    write(io, at, "src/a.rs", A);
    write(io, at, "src/b.rs", B);
}

/// blaze-world's fixture toolchain on the shelf of the home `home` (absolute; as main.rs's
/// tests place it): its id.
fn shelf(io: &Io, home: &str) -> blaze_toolchain::ToolchainId {
    let (id, files) = blaze_world::fixture::shelf(
        graph::TRIPLE,
        blaze_world::fixture::STD,
        crate::tests::SHELF_CFG,
        blaze_own::testing::heap().alloc(),
    );
    for (name, bytes) in files {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let path = Path::new(&format!("{}/{}/{name}", home.trim_start_matches('/'), toolchain::SHELF)).expect("a path");
        io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
    }
    id
}

fn stdout(io: &Io) -> std::string::String {
    let out = io.script(|host| host.out.clone()).expect("a simulated host");
    std::string::String::from_utf8(out).expect("UTF-8")
}

/// Whether a key may hold the checkout: the planted bug lets its path into the tool, which
/// every key holds (blaze-drive key.rs `task_key`), as a key that hashed `PWD` would.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Keys {
    FromText,
    PlantedPath,
}

fn producers_for(at: &str, keys: Keys) -> blaze_ids::producer::Producers {
    match keys {
        Keys::FromText => blaze_ids::producer::Producers::for_tests(b"test"),
        Keys::PlantedPath => blaze_ids::producer::Producers::for_tests(format!("test at /{at}").as_bytes()),
    }
}

/// `check` in the worktree `at`, started in `/` with `HOME=/h`, as `command` makes it.
fn check_in(io: &Io, at: &'static str, keys: Keys) -> Ran {
    check_with(io, at, keys, HOME, None)
}

/// `check` in the worktree `at`, started in `/` with `HOME=home`, `origin` named or not.
pub(super) fn check_with(io: &Io, at: &'static str, keys: Keys, home: &'static str, origin: Option<&super::NamedOrigin>) -> Ran {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new(at).expect("a path")).expect("open");
    let (out, err) = (io.need_out().expect("out"), io.need_err().expect("err"));
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    // A frame's header links to the file by its absolute path (main.rs, `PWD` joined).
    let shown_root: &'static str = if at == ONE { "/one" } else { "/two" };
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, shown_root, frame);
    let mut ran = None;
    let launch = program::Launch { io, select: crate::run_package::Select::NONE, grants: &[], args: &[] };
    let command = (Command::Check, "check");
    let said = words(command.0, &out, &err);
    let toolchain = shelf(io, home);
    let before = stdout(io).len();
    let heap = blaze_own::testing::heap();
    let opened = super::open_for_command(&cx, io, Some(home), &root, at, phases.clock(), &err, heap).expect("the one store opens");
    let finished = request(&cx, &root, (at, None, None, Some(home), None, Some("/"), toolchain, crate::sdk::NONE_IN_TESTS, Err(crate::fetch::ExeDirWhy::NotRead)), origin, (&opened.home, &opened.lent, opened.open_ns), command, 1, &producers_for(at, keys), work::Sizes::USUAL, &launch, &blaze_screen::Unwatched, frame, said, &err, &mut ran, &mut shown, &mut phases, heap, false)
        .expect("the request answers");
    let status = close(&cx, command.0, &finished, &shown, ran, 0, frame, said).ok().map(|status| status.0);
    super::after_answer_printed(&cx, opened.lent, false, phases.clock(), &err).expect("the commit after the answer");
    let printed = stdout(io);
    let findings = printed[before..].lines().filter(|line| line.contains("error[") || line.contains("warning[")).map(std::string::String::from).collect();
    let profile = &finished.answer.profile;
    Ran {
        runs: profile.runs(),
        certified: profile.certified,
        errors: finished.answer.findings,
        status,
        found: profile.hits,
        written: profile.puts,
        profile: profile.to_string(),
        findings,
    }
}

/// The `made` count of `kind`'s profile row.
fn made(profile: &str, kind: &str) -> u32 {
    let row = profile.lines().find(|line| line.split_whitespace().next() == Some(kind)).unwrap_or("");
    let words: std::vec::Vec<&str> = row.split_whitespace().collect();
    words.iter().position(|word| *word == "made").and_then(|at| words.get(at + 1)).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The profile rows whose kind made something.
fn made_rows(profile: &str) -> std::string::String {
    profile.lines().filter(|line| line.contains(" made ") && !line.split_whitespace().skip_while(|w| *w != "made").nth(1).is_some_and(|n| n == "0")).map(|line| format!("{line}\n")).collect()
}

/// (i) The first worktree's cold check, then the second's: the second runs no task, finds
/// what it needs in the machine's stores and writes nothing there; its answer equals a fresh
/// home's. `Err` says which clause failed (the planted test expects one).
fn second_worktree_runs_nothing(keys: Keys) -> Result<(), std::string::String> {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    worktree(&io, ONE);
    worktree(&io, TWO);
    let one = check_in(&io, ONE, keys);
    if one.runs == 0 || one.written == 0 {
        return Err(format!("the first worktree's cold check made nothing in the store: {} runs, {} written\n{}", one.runs, one.written, one.profile));
    }
    let two = check_in(&io, TWO, keys);
    let mut said = std::string::String::new();
    if two.runs != 0 {
        // First, within the eight lines the scripts show: which kinds made.
        write!(said, "made in two:\n{}made in one:\n{}", made_rows(&two.profile), made_rows(&one.profile)).expect("text");
    }
    if two.runs != 0 {
        writeln!(said, "the second worktree ran {} tasks the first had run", two.runs).expect("a string takes text");
    }
    if two.written != 0 || two.found == 0 {
        // The profile's rows on the same line: a runner quotes a failure's first lines.
        let rows = two.profile.lines().filter(|l| !l.contains(" made      0 ") || l.starts_with("store")).collect::<std::vec::Vec<_>>().join(" / ");
        writeln!(said, "the second worktree: store found {} written {} (wanted found > 0, written 0): {rows}", two.found, two.written).expect("a string takes text");
    }
    // A fresh home: the same files at the same place, nothing made before.
    let fresh_io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    worktree(&fresh_io, TWO);
    let fresh = check_in(&fresh_io, TWO, keys);
    if (two.errors, two.status, &two.findings) != (fresh.errors, fresh.status, &fresh.findings) {
        writeln!(said, "the second worktree's answer ({} errors, status {:?}, {:?}) is not a fresh home's ({} errors, status {:?}, {:?})", two.errors, two.status, two.findings, fresh.errors, fresh.status, fresh.findings).expect("a string takes text");
    }
    if fresh.errors == 0 {
        writeln!(said, "the fixture's unresolved import was not found: the comparison proves nothing").expect("a string takes text");
    }
    match said.is_empty() {
        true => Ok(()),
        false => {
            let err = io.script(|host| host.err.clone()).expect("a simulated host");
            let err = std::string::String::from_utf8_lossy(&err).into_owned();
            Err(format!("{said}--- the second worktree's profile:\n{}--- the first's:\n{}--- stderr:\n{err}", two.profile, one.profile))
        }
    }
}

#[test]
fn a_second_worktree_finds_every_record_and_answers_as_a_fresh_home() {
    if let Err(said) = second_worktree_runs_nothing(Keys::FromText) {
        panic!("{said}");
    }
}

/// (iii) The planted bug: the checkout's path in the keys. The second worktree must rebuild,
/// and the oracle above must name it.
#[test]
fn the_worktree_oracle_catches_a_path_in_the_keys() {
    match second_worktree_runs_nothing(Keys::PlantedPath) {
        Ok(()) => panic!("a path in the keys was not caught: the second worktree ran nothing"),
        Err(said) => assert!(said.contains("the second worktree ran") && said.contains("made in two:"), "caught otherwise: {said}"),
    }
}

/// (ii) An edit of one body in the second worktree reruns that body's check there and no
/// names; the first worktree's answer stands untouched (certified, no run), and the second's
/// next check stands too.
#[test]
fn an_edit_in_one_worktree_reruns_its_body_alone_and_leaves_the_other() {
    let io = Io::simulated(SimWorld::new(12, Limits::USUAL));
    worktree(&io, ONE);
    worktree(&io, TWO);
    let one = check_in(&io, ONE, Keys::FromText);
    let two = check_in(&io, TWO, Keys::FromText);
    assert_eq!(two.runs, 0, "the second worktree's cold check:\n{}", two.profile);
    write(&io, TWO, "src/b.rs", B_EDITED);
    let edited = check_in(&io, TWO, Keys::FromText);
    assert_eq!(
        (made(&edited.profile, "BodyCheck"), made(&edited.profile, "CrateScope"), edited.errors),
        (1, 0, one.errors),
        "a body edit in the second worktree reruns that body alone:\n{}",
        edited.profile
    );
    let again = check_in(&io, ONE, Keys::FromText);
    assert!(again.certified && again.runs == 0, "the first worktree after the second's edit: certified {}, {} runs:\n{}", again.certified, again.runs, again.profile);
    assert_eq!((again.errors, &again.findings), (one.errors, &one.findings), "the first worktree's answer moved");
    let repeat = check_in(&io, TWO, Keys::FromText);
    assert!(repeat.certified && repeat.runs == 0, "the second worktree's repeat: certified {}, {} runs:\n{}", repeat.certified, repeat.runs, repeat.profile);
}

/// (iv) Both worktrees warm, then one body edited in worktree two: its check runs exactly two
/// tasks, the edited file's outline (`FileItems`: a file's text is its key) and that body's
/// `BodyCheck`, and no names task (`CrateScope` made 0: the file's `Surface` did not move).
/// Worktree one's next check runs zero tasks and stays certified. Then worktree one takes the
/// same edit (the same text at another path): it runs zero tasks, both records found in the
/// machine's store, nothing written there. The first line is the cause this file's two older
/// tests failed on: worktree two's cold check found the names task's shape in the store before
/// its files' `Surface`s were listed, keyed its value with them absent, missed and reran it
/// (blaze-drive request.rs `value`, `walk_quiet`).
/// Prints: `test store::worktree_tests::a_body_edit_runs_two_tasks_there_and_none_in_the_other_worktree ... ok`.
#[test]
fn a_body_edit_runs_two_tasks_there_and_none_in_the_other_worktree() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    worktree(&io, ONE);
    worktree(&io, TWO);
    let one = check_in(&io, ONE, Keys::FromText);
    let two = check_in(&io, TWO, Keys::FromText);
    assert_eq!((two.runs, made(&two.profile, "CrateScope")), (0, 0), "the second worktree's cold check reran what the first made:\n{}", two.profile);
    // Warm: each worktree's repeat is its certificate.
    let warm = (check_in(&io, ONE, Keys::FromText), check_in(&io, TWO, Keys::FromText));
    assert!(warm.0.certified && warm.1.certified && warm.0.runs + warm.1.runs == 0, "the warm repeats: {} and {} runs", warm.0.runs, warm.1.runs);
    write(&io, TWO, "src/b.rs", B_EDITED);
    let edited = check_in(&io, TWO, Keys::FromText);
    assert_eq!(
        (edited.runs, made(&edited.profile, "FileItems"), made(&edited.profile, "BodyCheck"), made(&edited.profile, "CrateScope"), made(&edited.profile, "Surface")),
        (2, 1, 1, 0, 0),
        "a body edit in worktree two runs its file's outline and its body's check, nothing else:\n{}",
        edited.profile
    );
    // The edited body's types are checked although its crate's names record stands: front2
    // reads its file at item grain (item-now-1); before, two `types-not-checked` notes here.
    assert_eq!((edited.errors, &edited.findings), (two.errors, &two.findings), "a body edit moved worktree two's findings");
    let other = check_in(&io, ONE, Keys::FromText);
    assert!(other.certified && other.runs == 0, "worktree one after two's edit: certified {}, {} runs:\n{}", other.certified, other.runs, other.profile);
    // The same edit in worktree one: every record it needs is the one worktree two made.
    write(&io, ONE, "src/b.rs", B_EDITED);
    let adopted = check_in(&io, ONE, Keys::FromText);
    assert_eq!((adopted.runs, adopted.written), (0, 0), "worktree one, given the text worktree two built, ran or wrote:\n{}", adopted.profile);
    assert_eq!((adopted.errors, &adopted.findings), (one.errors, &one.findings), "the same edit moved worktree one's findings");
}

/// A workspace of one member under a `crates/*` glob, in the worktree at `at`.
fn workspace(io: &Io, at: &str) {
    write(io, at, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n");
    write(io, at, "crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, at, "crates/a/src/lib.rs", A);
}

/// (v) What git and cargo keep in a checkout's root is no input of its units: worktree one is
/// a clone (`.git` a directory) where cargo has run (`target/`), worktree two a `git worktree
/// add` (`.git` a file). Two's first check runs no task, `Units` and `UnitGraph` included. Its
/// controls, in two: a member folder made under the glob, then a manifest edited, each lay out
/// the units again. Without sweep.rs's filter of the root's listing the first assertion fails:
/// `Units` and `UnitGraph` made in two (2026-11-17, 45 / 71 / 129 ms on hello / gauntlet /
/// ripgrep for a second checkout's first check, against 15 / 37 / 87 for a third).
#[test]
fn a_worktree_and_a_built_clone_have_one_units_key() {
    let io = Io::simulated(SimWorld::new(14, Limits::USUAL));
    workspace(&io, ONE);
    write(&io, ONE, ".git/HEAD", "ref: refs/heads/main\n");
    write(&io, ONE, "target/debug/.cargo-lock", "");
    workspace(&io, TWO);
    write(&io, TWO, ".git", "gitdir: ../one/.git/worktrees/two\n");
    let one = check_in(&io, ONE, Keys::FromText);
    assert!(made(&one.profile, "Units") >= 1, "the clone's cold check laid out no units:\n{}", one.profile);
    let two = check_in(&io, TWO, Keys::FromText);
    assert_eq!(
        (two.runs, made(&two.profile, "Units"), made(&two.profile, "UnitGraph")),
        (0, 0, 0),
        "the worktree's first check laid out the clone's units again:\n{}",
        two.profile
    );
    write(&io, TWO, "crates/b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, TWO, "crates/b/src/lib.rs", B);
    let member = check_in(&io, TWO, Keys::FromText);
    assert!(made(&member.profile, "Units") >= 1, "control: a new member did not lay out the units:\n{}", member.profile);
    write(&io, TWO, "crates/b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.2.0\"\nedition = \"2024\"\n");
    let manifest = check_in(&io, TWO, Keys::FromText);
    assert!(made(&manifest.profile, "Units") >= 1, "control: an edited manifest did not lay out the units:\n{}", manifest.profile);
}
