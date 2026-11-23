//! A member build script's reads of the checkout as cells (engine_script_reads.rs), followed by
//! the real sweep over the simulated world. The script here is a fake with the sandbox's own
//! answers: it reads `stat:pkg`, `dir:pkg/data` and `file:pkg/data/in.txt`, each answered by
//! `engine_script_reads::now` cold (what the sandbox would record), and its outputs are a digest
//! of what it saw. A run is entered again exactly when a row of its reads moved; the CONTROL
//! (`ReadsPlant::FileDropped`) drops the `file:` read and misses the edit a fresh run sees.

use super::*;

use crate::engine_script_reads::{self as reads, CheckoutRead, NoCell, ReadKind, ReadsPlant};
use crate::sweep::{Follow, LeafKind, Leaves, Plant, Rows, Stamp, Stamps};

/// No stamp proves anything: every row is looked at again, as on a file system with no inode.
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

const PKG_READS: [(ReadKind, &str); 3] = [(ReadKind::Stat, "pkg"), (ReadKind::Listing, "pkg/data"), (ReadKind::File, "pkg/data/in.txt")];
const OTHER_READS: [(ReadKind, &str); 1] = [(ReadKind::Stat, "pkg")];
/// No row moved.
const NONE_MOVED: [(LeafKind, std::string::String); 0] = [];

fn world() -> Io {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n");
    write(&io, "pkg/build.rs", "fn main() {}\n");
    write(&io, "pkg/data/in.txt", "one\n");
    io
}

fn remove(io: &Io, path: &str) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    root.remove(&cx, &Path::new(path).expect("a path"), Durable::No).expect("remove");
}

/// What one run of the fake script gives: its read list as cells (through the sandbox's
/// subjects and `cells_of`, `plant` applied) and its outputs, a digest of everything it saw.
fn run(io: &Io, script: &[(ReadKind, &str)], plant: ReadsPlant) -> (reads::ReadList, blaze_ids::ContentId) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let frame = cx.frame();
    let mut recorded: std::vec::Vec<(std::string::String, Option<blaze_ids::ContentId>)> = std::vec::Vec::new();
    let mut saw: std::vec::Vec<CheckoutRead> = std::vec::Vec::new();
    for (kind, path) in script.iter() {
        let (_, _, answer) = reads::now(&cx, &root, &NoStamps, Plant::None, *kind, path, None, frame).expect("the read is answered");
        let word = match kind {
            ReadKind::File => "file",
            ReadKind::Listing => "dir",
            ReadKind::Stat => "stat",
            ReadKind::Git => "git",
        };
        // The sandbox records no digest for nothing there.
        recorded.push((format!("{word}:{path}"), (answer != reads::NONE).then_some(answer)));
        saw.push(CheckoutRead { kind: *kind, path: reads::Subject::from_str_in(path, blaze_own::testing::heap().shared()), answer });
    }
    let cells = reads::cells_of(recorded.iter().map(|(subject, digest)| (subject.as_str(), *digest)), plant).expect("every read has a cell");
    (cells, reads::outputs_of(&saw))
}

/// The leaf table a request saves for `cells`, read back as the next request reads it.
fn table(io: &Io, cells: &[CheckoutRead]) -> std::vec::Vec<u8> {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let mut rows = Rows::new_in(frame);
    reads::keep_rows(None, cells.iter().map(|read| (read.kind, read.path.as_str(), read.answer)), |_| true, &mut rows);
    Leaves::bytes_in(&rows, frame).expect("the table is written").to_vec()
}

/// The paths whose rows moved since `bytes`, by the real sweep.
fn moved(io: &Io, bytes: &[u8]) -> std::vec::Vec<(LeafKind, std::string::String)> {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let frame = cx.frame();
    let (last, older) = Leaves::of(bytes).expect("the table parses");
    assert!(older.is_none(), "the table is of this build's form");
    let followed = Follow { stamps: NoStamps, plant: Plant::None }.follow(&cx, &root, None, &last, frame).expect("the sweep follows");
    assert!(!followed.layout_moved, "a script's read never moves the layout");
    let mut out = std::vec::Vec::new();
    for i in 0..followed.now.rows.len() {
        if followed.changed(i) {
            let leaf = &followed.now.rows[i];
            out.push((leaf.kind, std::string::String::from(followed.now.path(leaf))));
        }
    }
    out
}

/// (a), (b): the first run's reads are cells, one row each in the leaf table, which reads back
/// with their kinds; with nothing changed no row moves (the certificate stands, no script runs),
/// and a second run sees the same answers and makes the same outputs.
#[test]
fn script_reads_are_cells_and_a_no_op_moves_none() {
    let io = world();
    let (cells, outputs) = run(&io, &PKG_READS, ReadsPlant::None);
    assert_eq!(cells.len(), 3, "each of stat, dir and file is a cell: {cells:?}");
    let bytes = table(&io, &cells);
    let (last, _) = Leaves::of(&bytes).expect("the table parses");
    let mut kinds: std::vec::Vec<LeafKind> = (0..last.len()).map(|k| last.row(k).kind).collect();
    kinds.sort();
    assert_eq!(kinds, [LeafKind::ScriptFile, LeafKind::ScriptListing, LeafKind::ScriptStat]);
    assert_eq!(moved(&io, &bytes), NONE_MOVED, "a no-op request moves no row");
    assert_eq!(run(&io, &PKG_READS, ReadsPlant::None).1, outputs, "the same answers, the same outputs");
}

/// (c): editing the file moves its row alone, named by its path; its `Given` task's key moves
/// with it, the folder's stat and the listing's do not.
#[test]
fn editing_a_read_file_moves_that_read_alone() {
    let io = world();
    let (cells, outputs) = run(&io, &PKG_READS, ReadsPlant::None);
    let bytes = table(&io, &cells);
    write(&io, "pkg/data/in.txt", "two\n");
    assert_eq!(moved(&io, &bytes), [(LeafKind::ScriptFile, std::string::String::from("pkg/data/in.txt"))]);
    let (again, outputs_again) = run(&io, &PKG_READS, ReadsPlant::None);
    assert_ne!(outputs_again, outputs, "the run entered again sees the edit");
    for (was, now) in cells.iter().zip(again.iter()) {
        assert_eq!(was.source() != now.source(), was.kind == ReadKind::File, "only the file's `Given` source moves: {was:?} against {now:?}");
    }
    // Another script that stat'd the package folder alone is not entered.
    let (other, _) = run(&io, &OTHER_READS, ReadsPlant::None);
    assert_eq!(moved(&io, &table(&io, &other)), NONE_MOVED);
}

/// (d): a file added under the listed folder moves the listing; one added beside it (in the
/// package folder, whose stat alone was read) moves nothing: a folder's stat is its kind.
#[test]
fn adding_a_file_moves_the_listing_only_beneath_it() {
    let io = world();
    let (cells, _) = run(&io, &PKG_READS, ReadsPlant::None);
    let bytes = table(&io, &cells);
    write(&io, "pkg/beside.txt", "x\n");
    assert_eq!(moved(&io, &bytes), NONE_MOVED, "a file beside data/ is read by no script");
    let stat = cells.iter().find(|read| read.kind == ReadKind::Stat).expect("the stat read");
    assert_eq!(stat.answer, blaze_proc_macro::script_sandbox::stat_answer(true, 0), "a folder's stat does not depend on what is in it");
    write(&io, "pkg/data/new.txt", "x\n");
    assert_eq!(moved(&io, &bytes), [(LeafKind::ScriptListing, std::string::String::from("pkg/data"))]);
}

/// (e): deleting the file moves its row to its own absent answer and the listing's; a script
/// that read only the package folder's stat is not entered, so a failure of the first stops
/// only what reads it.
#[test]
fn deleting_a_read_file_moves_its_readers_only() {
    let io = world();
    let (cells, _) = run(&io, &PKG_READS, ReadsPlant::None);
    let (other, _) = run(&io, &OTHER_READS, ReadsPlant::None);
    let (bytes, other_bytes) = (table(&io, &cells), table(&io, &other));
    remove(&io, "pkg/data/in.txt");
    let mut found = moved(&io, &bytes);
    found.sort();
    assert_eq!(found, [(LeafKind::ScriptFile, std::string::String::from("pkg/data/in.txt")), (LeafKind::ScriptListing, std::string::String::from("pkg/data"))]);
    assert_eq!(moved(&io, &other_bytes), NONE_MOVED, "the other script is not entered");
    let (again, _) = run(&io, &PKG_READS, ReadsPlant::None);
    let file = again.iter().find(|read| read.kind == ReadKind::File).expect("the file read");
    assert_eq!(file.answer, reads::NONE, "an absent path has its own answer");
}

/// (f) CONTROL: with the `file:` read dropped, the edit moves no row, so the run is not entered
/// and its kept outputs stand; a fresh world's run makes other outputs: the dropped read is
/// what catches the edit.
#[test]
fn control_a_dropped_file_read_misses_the_edit() {
    let io = world();
    let (planted, kept) = run(&io, &PKG_READS, ReadsPlant::FileDropped);
    assert_eq!(planted.len(), 2, "the planted bug drops the file read: {planted:?}");
    let (whole, _) = run(&io, &PKG_READS, ReadsPlant::None);
    let (planted_bytes, whole_bytes) = (table(&io, &planted), table(&io, &whole));
    write(&io, "pkg/data/in.txt", "two\n");
    assert_eq!(moved(&io, &planted_bytes), NONE_MOVED, "the planted table misses the edit");
    let (_, fresh) = run(&io, &PKG_READS, ReadsPlant::None);
    assert_ne!(fresh, kept, "a fresh run sees the edit the planted table missed: its kept outputs are stale");
    assert_eq!(moved(&io, &whole_bytes).len(), 1, "without the plant the edit moves the file's row");
}

/// A read with no cell is a typed refusal naming it; OUT_DIR and store-served reads never reach
/// here (the sandbox records none).
#[test]
fn reads_with_no_cell_are_refused_by_name() {
    // A successful git invocation has an answer even when its stdout is empty.
    let answer = Some(blaze_ids::ContentId([5; 32]));
    assert!(reads::parse("git:pkg\nrev-parse HEAD", answer).is_ok());
    assert!(reads::parse("git:pkg\nstatus --porcelain", answer).is_ok());
    assert!(matches!(reads::parse("git:pkg\nstatus --porcelain", None), Err(NoCell::Unobserved { .. })));
    assert!(matches!(reads::parse("git:pkg\nstatus  --porcelain", answer), Err(NoCell::Git { .. })));
    assert!(matches!(reads::parse("env:HOME", None), Err(NoCell::Kind { .. })));
    assert!(matches!(reads::parse("file:/etc/passwd", None), Err(NoCell::Path { .. })));
    assert!(matches!(reads::parse("file:pkg/../x", None), Err(NoCell::Path { .. })));
    let root = reads::parse("dir:", None).expect("the checkout's root is a path");
    assert_eq!((root.kind, root.path.as_str(), root.answer), (ReadKind::Listing, "", reads::NONE));
    assert_ne!(reads::cell_name(ReadKind::File, "a"), reads::cell_name(ReadKind::Stat, "a"), "a path's reads of two kinds are two cells");
}

/// A v2 index with one tracked path, f (100644, "a\n"), its TREE extension
/// and trailing SHA-1. HEAD names a loose commit of that tree. Neither the
/// fixture nor the oracle needs an installed git or a user's configuration.
pub(super) fn status_files(link: bool) -> std::vec::Vec<(std::string::String, std::vec::Vec<u8>)> {
    use blaze_build_script::git::Oid;
    let (mode, tree, commit, checksum) = if link {
        (0o120000, "8c49c088e07eb22abdb91ceb2fdfaf5a0bf3cda0", "2057df0b2b4d97921e6550f774c49d74f0098135", "26773b48995bdaa67a1eee8fba876211d795758a")
    } else {
        (0o100644, "3be22be77da4887e869c981806d8452f034dd014", "ebf714d841b7fd510630ee5965b2ef0bdc3b5c08", "7c818220f432877458e9a7c7d5aea57cc66c3ee8")
    };
    let mut index = b"DIRC".to_vec();
    for field in [2u32, 1, 0, 0, 0, 0, 0, 0, mode, 0, 0, 2] {
        index.extend_from_slice(&field.to_be_bytes());
    }
    index.extend_from_slice(&Oid::parse_hex(b"78981922613b2afb6025042ff6bd878ac1994e85").unwrap().0);
    index.extend_from_slice(b"\0\x01f\0TREE\0\0\0\x19\x001 0\n");
    index.extend_from_slice(&Oid::parse_hex(tree.as_bytes()).unwrap().0);
    index.extend_from_slice(&Oid::parse_hex(checksum.as_bytes()).unwrap().0);
    let body = format!("tree {tree}\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nfixture\n");
    let alloc = blaze_own::testing::heap().alloc();
    let mut object = blaze_own::Vec::new_in(alloc);
    blaze_deflate::zlib(format!("commit {}\0{body}", body.len()).as_bytes(), blaze_deflate::Level::Fast, alloc, &mut object).unwrap();
    vec![
        (".git/HEAD".into(), format!("{commit}\n").into_bytes()),
        (format!(".git/objects/{}/{}", &commit[..2], &commit[2..]), object.to_vec()),
        (".git/index".into(), index),
        ("f".into(), b"a\n".to_vec()),
    ]
}

fn porcelain(stdout: &[u8]) -> blaze_ids::ContentId {
    let mut reply = 0i32.to_le_bytes().to_vec();
    reply.extend_from_slice(&(stdout.len() as u64).to_le_bytes());
    reply.extend_from_slice(stdout);
    reply.extend_from_slice(&0u64.to_le_bytes()); // stderr
    blaze_proc_macro::script_sandbox::file_answer(&reply)
}

const PORCELAIN: &str = "one\nstatus --porcelain";
const TRACKED: &str = "two\nstatus --porcelain --untracked-files=no";

fn status_history(tape: &blaze_sim::Tape, plant: ReadsPlant) -> Result<(), blaze_sim::Broke> {
    use blaze_proc_macro::script_sandbox::{answer_now, mem_tree};
    let io = world();
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let base = status_files(false);
    let mut previous: Option<(std::vec::Vec<u8>, u64)> = None;
    let mut previous_all: Option<(std::vec::Vec<u8>, blaze_ids::ContentId)> = None;
    // Always start clean, repeat, edit twice with the same porcelain line,
    // revert, create an untracked file, delete it, then vary those states.
    for tick in 0..12 {
        let state = match tick {
            0 | 1 | 4 | 6 => 0,
            2 => 1,
            3 => 2,
            5 => 3,
            _ => tape.below(4, "worktree edit"),
        };
        let mut files = base.clone();
        let contents: &[u8] = match state { 1 => b"b\n", 2 => b"c\n", _ => b"a\n" };
        files.iter_mut().find(|(name, _)| name == "f").unwrap().1 = contents.to_vec();
        if state == 3 {
            files.push(("new".into(), b"x".to_vec()));
        }
        let tree = mem_tree(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())), "", None);
        let mut sweep = reads::CheckoutSweep::new(tree.clone(), blaze_own::testing::heap().alloc(), frame);
        let mut status_bytes = if contents == b"a\n" { b"".to_vec() } else { b" M f\n".to_vec() };
        if state == 3 {
            status_bytes.extend_from_slice(b"?? new\n");
        }
        let expected = porcelain(&status_bytes);
        let tracked_expected = porcelain(if contents == b"a\n" { b"" } else { b" M f\n" });
        let mut saved_tracked = None;
        for (ask, wanted) in [(TRACKED, Some(tracked_expected)), (PORCELAIN, Some(expected))] {
            let subject = format!("git:{ask}");
            let sandbox = answer_now(&tree, &subject).expect("a supported question");
            assert_eq!(sandbox, wanted, "porcelain answer in state {state}");
            let (folder, command) = ask.split_once('\n').unwrap();
            let binding = format!("git-scope:{folder}");
            let work = blaze_proc_macro::script_sandbox::status_scope(&tree, folder, blaze_own::testing::heap().alloc());
            assert_eq!(work, "");
            let canonical = format!("git:{work}\n{command}");
            if ask == PORCELAIN && let Some((bytes, old_answer)) = previous_all.take() {
                let (last, _) = Leaves::of(&bytes).expect("the default porcelain run's saved rows");
                let moved = (0..last.len()).any(|i| sweep.row(ReadKind::Git, last.path(i)).unwrap().2 != last.row(i).id);
                assert_eq!(moved, sandbox != Some(old_answer), "an untracked file breaks a clean certificate");
            }
            let mut cells = reads::ReadList::new_in(blaze_own::testing::heap().shared());
            if sandbox.is_none() {
                assert_eq!(sweep.answer(ask).unwrap(), reads::NONE);
                assert!(matches!(reads::collect_reads([(canonical.as_str(), None)].into_iter(), plant, Some(sweep.view()), &mut cells), Err(NoCell::Unobserved { .. })));
                continue;
            }
            let observed = [(binding.as_str(), answer_now(&tree, &binding).unwrap()), (canonical.as_str(), sandbox)];
            if reads::collect_reads(observed.into_iter(), plant, Some(sweep.view()), &mut cells).is_err() {
                return Err(blaze_sim::Broke::Oracle("an observed status was always owed".into()));
            }
            for read in &cells {
                assert_eq!(sweep.row(read.kind, &read.path).unwrap().2, read.answer);
            }
            if ask == TRACKED {
                saved_tracked = Some(table(&io, &cells));
            } else {
                previous_all = Some((table(&io, &cells), sandbox.unwrap()));
            }
        }
        // A second package asks the first question in the same checkout.
        assert_eq!(sweep.answer("three\nstatus --porcelain --untracked-files=no").unwrap(), tracked_expected);
        assert_eq!(sweep.status_questions(), 2, "one answer per repository and untracked mode");
        if let Some((old, old_state)) = previous {
            let (last, _) = Leaves::of(&old).expect("a restored table");
            let moved = (0..last.len()).any(|i| sweep.row(ReadKind::Git, last.path(i)).unwrap().2 != last.row(i).id);
            assert_eq!(moved, matches!(state, 1 | 2) != matches!(old_state, 1 | 2));
            if !moved {
                assert_eq!(&old, saved_tracked.as_ref().unwrap(), "standing and fresh leaf records have identical bytes");
            }
        }
        previous = Some((saved_tracked.expect("the tracked-only script had a complete run"), state));
    }
    Ok(())
}

#[test]
fn status_cells_stand_on_the_served_porcelain_answer() {
    blaze_sim::check(blaze_sim::Config::cases(24), |tape| status_history(tape, ReadsPlant::None));
}

#[test]
fn control_an_always_owed_status_is_caught() {
    blaze_sim::control(blaze_sim::Config::cases(24), ReadsPlant::None, ReadsPlant::StatusAlwaysOwed, "always owed", status_history);
}

#[test]
fn a_new_nested_worktree_rebinds_only_the_scripts_beneath_it() {
    use blaze_proc_macro::script_sandbox::{answer_now, mem_tree, scope_answer};
    let io = world();
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let alloc = blaze_own::testing::heap().alloc();
    let mut files = status_files(false);
    files.iter_mut().find(|(name, _)| name == "f").unwrap().1 = b"b\n".to_vec();
    for nested in [false, true] {
        if nested {
            files.extend(status_files(false).into_iter().map(|(name, bytes)| (format!("pkg/{name}"), bytes)));
        }
        let tree = mem_tree(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())), "", None);
        let mut sweep = reads::CheckoutSweep::new(tree.clone(), alloc, cx.frame());
        for (folder, work) in [("pkg/sub", if nested { "pkg" } else { "" }), ("other", "")] {
            let binding = format!("git-scope:{folder}");
            assert_eq!(answer_now(&tree, &binding).unwrap(), Some(scope_answer(work)));
            assert_eq!(sweep.row(ReadKind::Git, folder).unwrap().2, scope_answer(work));
            let asked = format!("{folder}\nstatus --porcelain --untracked-files=no");
            let canonical = format!("{work}\nstatus --porcelain --untracked-files=no");
            let answer = porcelain(if work.is_empty() { b" M f\n" } else { b"" });
            assert_eq!(answer_now(&tree, &format!("git:{asked}")).unwrap(), Some(answer));
            assert_eq!(sweep.answer(&canonical).unwrap(), answer);
        }
        assert_eq!(sweep.status_questions(), if nested { 2 } else { 1 });
    }
}

#[test]
fn a_pinned_tree_covers_only_observations_named_by_its_grant() {
    let alloc = blaze_own::testing::heap().shared();
    let file = Some(blaze_proc_macro::script_sandbox::file_answer(b"private"));
    let valid = [("file:header.h", file), ("stat:missing", None)];
    assert_eq!(reads::pinned_reads(valid.into_iter(), alloc), Ok(()));
    for (subject, variant) in [("changed-during-run:file:header.h", "Conflicting"), ("unknown:header.h", "Kind"), ("git:pkg\nrev-parse HEAD", "Unobserved")] {
        let rejected = reads::pinned_reads(valid.into_iter().chain([(subject, None)]), alloc).unwrap_err();
        let expected = subject.strip_prefix("changed-during-run:").unwrap_or(subject);
        assert_eq!(rejected.first_rejected(), (variant, expected));
    }
}

#[cfg(unix)]
#[test]
fn file_replay_distinguishes_a_revoked_grant_from_absence() {
    use std::fs;
    use std::os::unix::fs::symlink;
    use blaze_proc_macro::script_sandbox::{RealRoot, TreeRead, dir_tree};
    let dir = std::env::temp_dir().join(format!("blaze-script-grant-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let checkout = dir.join("checkout");
    fs::create_dir(&checkout).unwrap();
    fs::write(dir.join("private"), b"private").unwrap();
    let tree = dir_tree(&RealRoot::resolve(&checkout).expect("the fixture's folder resolves"));
    let io = world();
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let mut sweep = reads::CheckoutSweep::new(tree.clone(), blaze_own::testing::heap().alloc(), cx.frame());
    assert_eq!(sweep.row(ReadKind::File, "input").unwrap().2, reads::NONE);
    symlink("../private", checkout.join("input")).unwrap();
    assert!(TreeRead::File.answer(&tree, "input").is_err());
    let denied = sweep.row(ReadKind::File, "input").unwrap().2;
    assert_ne!(denied, reads::NONE);
    assert_ne!(denied, blaze_proc_macro::script_sandbox::file_answer(b"private"));
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn status_replay_observes_index_mode_links_and_root_entries() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use blaze_proc_macro::script_sandbox::{RealRoot, answer_now, dir_tree};
    let dir = std::env::temp_dir().join(format!("blaze-script-status-{}", std::process::id()));
    fs::create_dir(&dir).expect("a private fixture folder");
    let tree = dir_tree(&RealRoot::resolve(&dir).expect("the fixture's folder resolves"));
    let io = world();
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let install = |link| {
        for (name, bytes) in status_files(link) {
            let path = dir.join(&name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if link && name == "f" {
                symlink("a\n", path).unwrap();
            } else {
                fs::write(path, bytes).unwrap();
            }
        }
    };
    let check = |asked: &str, expected| {
        let sandbox = answer_now(&tree, &format!("git:{asked}")).unwrap();
        assert_eq!(sandbox, expected, "{asked}");
        let mut sweep = reads::CheckoutSweep::new(tree.clone(), blaze_own::testing::heap().alloc(), cx.frame());
        assert_eq!(sweep.answer(asked).unwrap(), sandbox.unwrap_or(reads::NONE));
    };
    install(false);
    let clean = Some(porcelain(b""));
    let dirty = Some(porcelain(b" M f\n"));
    check(PORCELAIN, clean);
    fs::set_permissions(dir.join("f"), fs::Permissions::from_mode(0o755)).unwrap();
    check(TRACKED, dirty);
    fs::write(dir.join("new"), b"x").unwrap();
    check(PORCELAIN, Some(porcelain(b" M f\n?? new\n")));
    check(TRACKED, dirty);
    fs::remove_file(dir.join("new")).unwrap();
    check(PORCELAIN, dirty);
    fs::remove_file(dir.join("f")).unwrap();
    install(true);
    check(PORCELAIN, clean);
    fs::remove_file(dir.join("f")).unwrap();
    symlink("b\n", dir.join("f")).unwrap();
    check(TRACKED, dirty);
    fs::write(dir.join(".git/index"), b"not an index").unwrap();
    check(TRACKED, None);
    fs::remove_dir_all(dir).unwrap();
}

/// When `Stage::Scripts` enters only `Given` tasks (0 `Script` runs), `Scripts` finishes its
/// synchronous `Given` cells before admitting `Stage::Names` and reads 0 crate records (0 bytes),
/// while `Stage::Names` still opens its dependency exports and matches a fresh oracle.
#[test]
fn scripts_with_only_given_tasks_reads_zero_crate_records() {
    use blaze_engine::Stage;

    const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
    const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
    const A_LIB: &str = "#![no_core]\npub struct Tag;\n";
    const B_LIB: &str = "#![no_core]\n\
        extern crate a;\n\
        #[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
        pub fn use_a(x: a::Tag) -> a::Tag { x }\n\
        pub fn target() { let _ = include_str!(concat!(\"../assets/\", \"g.json\")); }\n";
    const B_LIB_EDITED: &str = "#![no_core]\n\
        extern crate a;\n\
        #[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
        pub fn use_a(x: a::Tag) -> a::Tag { x }\n\
        pub fn target() { let _ = include_str!(concat!(\"../assets/\", \"g.json\")); }\n\
        fn _private_helper() {}\n";

    let build_world = |b_lib: &str, data: &str| {
        let io = Io::simulated(SimWorld::new(83, Limits::USUAL));
        write(&io, "Cargo.toml", WORKSPACE);
        write(&io, "a/Cargo.toml", A_MANIFEST);
        write(&io, "a/src/lib.rs", A_LIB);
        write(&io, "b/Cargo.toml", B_MANIFEST);
        write(&io, "b/src/lib.rs", b_lib);
        write(&io, "b/assets/g.json", data);
        io
    };

    for workers in [1, 2] {
        let io = build_world(B_LIB, "one\n");
        let (cold_ids, _) = super::engine_check_tests::engine_from(&io, workers, false);
        assert!(cold_ids.is_empty(), "cold check clean: {cold_ids:?}");

        // Edit the computed data include (enters 1 Given task in Stage::Scripts, 0 Script tasks)
        // AND add a private fn in `b` (enters `b`'s CrateScope in Stage::Names, which reads `a`).
        write(&io, "b/assets/g.json", "two\n");
        write(&io, "b/src/lib.rs", B_LIB_EDITED);
        let (ids, checked) = super::engine_check_tests::engine_from(&io, workers, false);

        assert_eq!(checked.stages[Stage::Scripts.index()].entered, 1, "workers={workers}: exactly the 1 Given task enters Scripts");
        assert!(checked.stages[Stage::Names.index()].entered >= 1, "workers={workers}: b's CrateScope enters Names");
        let script_reads = checked.records.reads[Stage::Scripts.index()];
        assert_eq!(
            (script_reads.records, script_reads.bytes),
            (0, 0),
            "workers={workers}: Scripts with only Given tasks must read 0 crate records: {script_reads:?}"
        );
        let names_reads = checked.records.reads[Stage::Names.index()];
        assert!(
            names_reads.records >= 1,
            "workers={workers}: Names must read b's dependency exports itself: {names_reads:?}"
        );

        let (fresh_ids, fresh) = super::engine_check_tests::engine_from(&build_world(B_LIB_EDITED, "two\n"), workers, true);
        assert_eq!(ids, fresh_ids, "workers={workers}: incremental answer matches fresh oracle");
        assert_eq!(checked.status.0, fresh.status.0, "workers={workers}: incremental status matches fresh oracle");
    }
}
