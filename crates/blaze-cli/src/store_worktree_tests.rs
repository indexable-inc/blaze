//! Two worktrees of one repository on one machine (remote-read.md, "Shared across worktrees
//! (2026-11-16)"): two folders with the same files at different paths, one home. Every drive
//! record is keyed by text alone (rules 25 and 71), so the second worktree's cold `check` finds
//! every record the first made in the user's own store and runs no task; its findings equal a
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
    pub(super) machine_found: u32,
    pub(super) machine_put: u32,
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
    let (id, files) = blaze_world::fixture::shelf(graph::TRIPLE, blaze_world::fixture::STD, &[("target_os", Some("macos")), ("unix", None)]);
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

fn tool_for(at: &str, keys: Keys) -> Tool {
    match keys {
        Keys::FromText => home::tool(b"test"),
        Keys::PlantedPath => home::tool(format!("test at /{at}").as_bytes()),
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
    let launch = program::Launch { io, grants: &[], args: &[] };
    let command = (Command::Check, "check");
    let said = words(command.0, &out, &err);
    let toolchain = shelf(io, home);
    let before = stdout(io).len();
    let finished = request(&cx, &root, (at, None, None, Some(home), None, Some("/"), toolchain), origin, command, 1, tool_for(at, keys), work::Sizes::USUAL, &launch, frame, said, &err, &mut ran, &mut shown, &mut phases)
        .expect("the request answers");
    let status = close(&cx, command.0, &finished, &shown, ran, 0, frame, said).ok().map(|status| status.0);
    let printed = stdout(io);
    let findings = printed[before..].lines().filter(|line| line.contains("error[") || line.contains("warning[")).map(std::string::String::from).collect();
    let profile = &finished.answer.profile;
    Ran {
        runs: profile.runs(),
        certified: profile.certified,
        errors: finished.answer.findings,
        status,
        machine_found: profile.machine_found,
        machine_put: profile.machine_put,
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

/// Every whole drive record in the user's own store under [`HOME`]: (key, name, reads).
fn own_records(io: &Io) -> std::vec::Vec<(blaze_drive::Key, blaze_drive::Name, std::vec::Vec<(blaze_drive::Name, blaze_ids::ContentId)>)> {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    let home = match super::Machine::of(&cx, &fs, Some(HOME)).expect("no harness error") {
        super::Machine::Home(home) => home,
        super::Machine::Off(why) => panic!("the simulation's home: {why}"),
    };
    let own = super::PackStore::open_in(&cx, &home, super::OWN, super::Boot::Unknown, super::PACKS_MOST, super::StorePlant::None).expect("the own store");
    let scratch = bstd::mem::Frame::with_reserve(TASK_FRAME, bstd::mem::Tag::Other).expect("a frame");
    let mut all = std::vec::Vec::new();
    own.each_record(&cx, &scratch, &mut |key, bytes| {
        if let Ok(blaze_drive::record::Stored::Whole(view)) = blaze_drive::record::open(bytes) {
            all.push((key, view.name, view.reads().collect()));
        }
        Ok(())
    })
    .expect("the own store's packs read");
    all
}

fn short(id: &blaze_ids::ContentId) -> std::string::String {
    id.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// For each whole record the second worktree wrote to the own store: the reads in which it
/// differs from the first worktree's record of the same name (what moved its value key).
fn moved_reads(before: &[(blaze_drive::Key, blaze_drive::Name, std::vec::Vec<(blaze_drive::Name, blaze_ids::ContentId)>)], after: &[(blaze_drive::Key, blaze_drive::Name, std::vec::Vec<(blaze_drive::Name, blaze_ids::ContentId)>)]) -> std::string::String {
    let mut said = std::string::String::new();
    for (key, name, reads) in after.iter().filter(|(key, _, _)| !before.iter().any(|(k, _, _)| k == key)) {
        let Some((_, _, old)) = before.iter().find(|(_, n, _)| n == name) else {
            writeln!(said, "new {name} at {key}: no record of that name from worktree one").expect("text");
            continue;
        };
        let mut diffs = std::vec::Vec::new();
        for (read, saw) in reads.iter() {
            match old.iter().find(|(r, _)| r == read) {
                Some((_, was)) if was == saw => {}
                Some((_, was)) => diffs.push(format!("{read} saw {} (one: {})", short(saw), short(was))),
                None => diffs.push(format!("{read} read only in two")),
            }
        }
        for (read, _) in old.iter().filter(|(r, _)| !reads.iter().any(|(n, _)| n == r)) {
            diffs.push(format!("{read} read only in one"));
        }
        writeln!(said, "new {name}: {} reads; differs: {}", reads.len(), if diffs.is_empty() { std::string::String::from("none (same reads: task key moved)") } else { diffs.join("; ") }).expect("text");
    }
    said
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
    if one.runs == 0 || one.machine_put == 0 {
        return Err(format!("the first worktree's cold check made nothing in the machine's stores: {} runs, {} written\n{}", one.runs, one.machine_put, one.profile));
    }
    let before = own_records(&io);
    let two = check_in(&io, TWO, keys);
    let after = own_records(&io);
    let mut said = std::string::String::new();
    if two.runs != 0 {
        // First, within the eight lines the scripts show: what moved, and which kinds made.
        said.push_str(&moved_reads(&before, &after));
        write!(said, "made in two:\n{}made in one:\n{}", made_rows(&two.profile), made_rows(&one.profile)).expect("text");
    }
    if two.runs != 0 {
        writeln!(said, "the second worktree ran {} tasks the first had run", two.runs).expect("a string takes text");
    }
    if two.machine_put != 0 || two.machine_found == 0 {
        writeln!(said, "the second worktree: machine found {} written {} (wanted found > 0, written 0)", two.machine_found, two.machine_put).expect("a string takes text");
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
        Err(said) => assert!(said.contains("the second worktree ran") && said.contains("new "), "caught otherwise: {said}"),
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
    assert_eq!((edited.errors, &edited.findings), (two.errors, &two.findings), "a body edit moved worktree two's findings");
    let other = check_in(&io, ONE, Keys::FromText);
    assert!(other.certified && other.runs == 0, "worktree one after two's edit: certified {}, {} runs:\n{}", other.certified, other.runs, other.profile);
    // The same edit in worktree one: every record it needs is the one worktree two made.
    write(&io, ONE, "src/b.rs", B_EDITED);
    let adopted = check_in(&io, ONE, Keys::FromText);
    assert_eq!((adopted.runs, adopted.machine_put), (0, 0), "worktree one, given the text worktree two built, ran or wrote:\n{}", adopted.profile);
    assert_eq!((adopted.errors, &adopted.findings), (one.errors, &one.findings), "the same edit moved worktree one's findings");
}
