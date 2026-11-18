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
fn run(io: &Io, script: &[(ReadKind, &str)], plant: ReadsPlant) -> (std::vec::Vec<CheckoutRead>, blaze_ids::ContentId) {
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
        saw.push(CheckoutRead { kind: *kind, path: std::string::String::from(*path), answer });
    }
    let cells = reads::cells_of(recorded.iter().map(|(subject, digest)| (subject.as_str(), *digest)), plant).expect("every read has a cell");
    (cells, reads::outputs_of(&saw))
}

/// The leaf table a request saves for `cells`, read back as the next request reads it.
fn table(io: &Io, cells: &[CheckoutRead]) -> std::vec::Vec<u8> {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let mut rows = Rows::new_in(frame);
    reads::keep_rows(None, cells.iter(), |_| true, &mut rows);
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
    // A git fact read from `.git` files has a cell (lane NR2); `status` reads every tracked
    // file's stat and has none yet.
    assert!(reads::parse("git:pkg\nrev-parse HEAD", None).is_ok(), "a git fact has a cell");
    assert!(matches!(reads::parse("git:pkg\nstatus --porcelain", None), Err(NoCell::Git { .. })));
    assert!(matches!(reads::parse("env:HOME", None), Err(NoCell::Kind { .. })));
    assert!(matches!(reads::parse("file:/etc/passwd", None), Err(NoCell::Path { .. })));
    assert!(matches!(reads::parse("file:pkg/../x", None), Err(NoCell::Path { .. })));
    let root = reads::parse("dir:", None).expect("the checkout's root is a path");
    assert_eq!((root.kind, root.path.as_str(), root.answer), (ReadKind::Listing, "", reads::NONE));
    assert_ne!(reads::cell_name(ReadKind::File, "a"), reads::cell_name(ReadKind::Stat, "a"), "a path's reads of two kinds are two cells");
}
