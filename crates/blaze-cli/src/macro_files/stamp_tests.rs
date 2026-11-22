//! A member proc-macro crate's rows followed by their stamps (piece member-macro-stamp; rules 21
//! and 77), on bstd's simulated file system with a model of the file times.
//!
//! The crate `mac` (`Cargo.toml`, `src/lib.rs`, `src/expand/mod.rs`, with `target/` and `.git/`
//! beneath it that the files digest leaves out and `.data/` and `src/target/` that it walks,
//! beside `app/src/main.rs` outside it) is read once
//! as the layout reads it ([`read_row`]): one `MacroFolder` row a folder, one `MacroFile` row a
//! file, and the `MacroFiles` row whose id is `blaze_proc_macro::module::files_digest` of them.
//! Then one step, the clock past the stamps' granularity, and the sweep (`Follow::follow`). The
//! bytes the sweep reads are counted by the simulated disk (`Io::disk().read`), the IO the
//! sweep is handed. When the sweep says the layout moved, the digest after is the layout's
//! again, as main.rs reads it (`units::lay` calls [`read_row`]).
//!
//! The oracle of a digest is `files_digest` over the model's content: every file under `mac/`
//! by its crate-relative path, but those under the crate's root `target` or under a `.git`
//! (the one package rule, blaze-build-script `served::in_package`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap as StdMap;
use std::string::{String, ToString};

use bstd::fs::{Content, Durable, Meta};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::{member_hex, read_row};
use crate::sweep::{Follow, Leaves, Plant, Rows, Stamp, Stamps};

const TICK: u64 = 1_000;
/// Past the granularity: every stamp written before is settled.
const SETTLE: u64 = 3 * TICK;
const LIB: &str = "mac/src/lib.rs";
const FILES: [(&str, &[u8]); 8] = [
    ("mac/Cargo.toml", b"[package]\nname = \"mac\"\n[lib]\nproc-macro = true\n"),
    (LIB, b"mod expand;\n#[proc_macro]\npub fn same(input: TokenStream) -> TokenStream { input }\n"),
    ("mac/src/expand/mod.rs", b"pub fn nothing() {}\n"),
    ("mac/target/debug/junk", b"left out: a folder named target at the crate's root"),
    ("mac/.git/HEAD", b"left out: a version-control folder"),
    // The twins: walked, an input of the key (the one package rule, served.rs `in_package`).
    ("mac/.data/table.txt", b"walked: a dot-folder that is no VCS's"),
    ("mac/src/target/mod.rs", b"walked: a folder named target beneath the root"),
    ("app/src/main.rs", b"fn main() {}\n"),
];

/// File and folder times, as a file system keeps them: a file's moves when it is written or
/// touched; a folder's when an entry is made in it (not when an entry's bytes change).
struct Model {
    now: Cell<u64>,
    times: RefCell<StdMap<String, u64>>,
}

impl Stamps for &Model {
    fn stamp(&self, path: &str, meta: &Meta) -> Option<Stamp> {
        let at = *self.times.borrow().get(path)?;
        Some(Stamp { len: meta.len, mtime_ns: at, ctime_ns: at, ino: 7, dev: 1 })
    }

    fn swept_at_ns(&self) -> u64 {
        self.now.get()
    }

    fn granularity_ns(&self) -> u64 {
        TICK
    }
}

/// What a test does between the layout's read and the sweep.
#[derive(Clone, Copy)]
enum Step {
    Nothing,
    /// `mac/src/lib.rs` given one byte more.
    Edit,
    /// `mac/src/lib.rs`'s time moved, its bytes the same.
    Touch,
    /// `mac/src/expand/more.rs` made.
    Add,
}

/// What the sweep did, and the digests.
struct Seen {
    /// Bytes the sweep read.
    read: u64,
    layout_moved: bool,
    before: String,
    after: String,
    /// The oracle's digest of the crate after the step.
    oracle: String,
    lib_len: u64,
}

fn oracle(content: &StdMap<String, std::vec::Vec<u8>>) -> String {
    let mut files: std::vec::Vec<(String, std::vec::Vec<u8>)> = std::vec::Vec::new();
    for (path, bytes) in content.iter() {
        let Some(rel) = path.strip_prefix("mac/") else { continue };
        // Written out, not asked of `in_package`: the crate's root `target`, and any `.git`.
        let folders = rel.split('/').collect::<std::vec::Vec<_>>();
        let left_out = (folders.len() > 1 && folders[0] == "target") || folders[..folders.len() - 1].contains(&".git");
        if !left_out {
            files.push((rel.to_string(), bytes.clone()));
        }
    }
    blaze_proc_macro::module::files_digest(&mut files)
}

fn run(step: Step) -> Seen {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("the file system");
    let root = fs.open_dir(&cx, &Path::new("data").expect("a path")).expect("the checkout");
    let model = Model { now: Cell::new(10 * TICK), times: RefCell::new(StdMap::new()) };
    let content: RefCell<StdMap<String, std::vec::Vec<u8>>> = RefCell::new(StdMap::new());
    let write = |path: &str, bytes: &[u8]| {
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(bytes), Durable::No).expect("a write");
        let now = model.now.get() - model.now.get() % TICK;
        let mut times = model.times.borrow_mut();
        // Each folder an entry is made in moves (the checkout's root is never stamped).
        let mut parent: Option<&str> = None;
        for (end, _) in path.match_indices('/').chain([(path.len(), "")]) {
            let child = &path[..end];
            if !times.contains_key(child)
                && let Some(parent) = parent
            {
                times.insert(parent.to_string(), now);
            }
            parent = Some(child);
        }
        times.insert(path.to_string(), now);
        content.borrow_mut().insert(path.to_string(), bytes.to_vec());
    };
    for (path, bytes) in FILES {
        write(path, bytes);
    }
    model.now.set(model.now.get() + SETTLE);
    let frame = cx.frame();
    let mut first = Rows::new_in(frame);
    read_row(&cx, &root, &&model, "mac", &mut first, frame).expect("the crate's rows");
    let before = member_hex(&first, "mac").expect("the crate's digest");
    assert_eq!(before, oracle(&content.borrow()), "the layout's digest is the files digest of the crate");
    let table = Leaves::bytes_in(&first, frame).expect("the leaf table").to_vec();
    model.now.set(model.now.get() + SETTLE);
    match step {
        Step::Nothing => {}
        Step::Edit => {
            let mut bytes = content.borrow()[LIB].clone();
            bytes.push(b'\n');
            write(LIB, &bytes);
        }
        Step::Touch => {
            let now = model.now.get() - model.now.get() % TICK;
            model.times.borrow_mut().insert(LIB.to_string(), now);
        }
        Step::Add => write("mac/src/expand/more.rs", b"pub fn more() {}\n"),
    }
    model.now.set(model.now.get() + SETTLE);
    let (last, older) = Leaves::of(&table).expect("the table read back");
    assert!(older.is_none(), "the table is this build's form");
    let reads = || io.disk().expect("a simulated disk").read;
    let was = reads();
    let followed = Follow { stamps: &model, plant: Plant::None }.follow(&cx, &root, None, &last, frame).expect("the sweep");
    let read = reads() - was;
    let after = match followed.layout_moved {
        // main.rs lays the workspace again, which reads the crate's rows again.
        true => {
            let mut laid = Rows::new_in(frame);
            read_row(&cx, &root, &&model, "mac", &mut laid, frame).expect("the crate's rows again");
            member_hex(&laid, "mac").expect("the crate's digest again")
        }
        false => member_hex(&followed.now, "mac").expect("the followed digest"),
    };
    let lib_len = content.borrow()[LIB].len() as u64;
    Seen { read, layout_moved: followed.layout_moved, before, after, oracle: oracle(&content.borrow()), lib_len }
}

/// An unchanged member macro crate costs the sweep one stat a file and a folder and no read
/// (rule 77): its digest stands because every `MacroFile` and `MacroFolder` row beneath it does.
/// Before member-macro-stamp the sweep read every file of the crate each command (the -2 row had
/// no stamp): this read the crate's 150 bytes and fails on `read 0`.
/// Prints `unchanged: read 0 bytes, digest <hex>`.
#[test]
fn an_unchanged_member_macro_crate_is_read_no_byte() {
    let seen = run(Step::Nothing);
    assert_eq!(seen.read, 0, "an unchanged member macro crate: the sweep read {} bytes", seen.read);
    assert!(!seen.layout_moved, "nothing changed, yet the layout moved");
    assert_eq!(seen.after, seen.before, "nothing changed, yet the digest moved");
    println!("unchanged: read {} bytes, digest {}", seen.read, seen.after);
}

/// One byte more in `mac/src/lib.rs` gives the crate another digest, the files digest of its
/// files today (rule 21): the file's stamp moved, the sweep read it, its id moved, so the crate's
/// digest no longer stands and the layout reads the crate again.
/// Prints `edited: <before> then <after>`.
#[test]
fn an_edit_of_a_member_macro_file_gives_a_new_digest() {
    let seen = run(Step::Edit);
    assert!(seen.layout_moved, "an edit of mac/src/lib.rs did not move the layout");
    assert_ne!(seen.after, seen.before, "an edit of mac/src/lib.rs left the crate's digest standing");
    assert_eq!(seen.after, seen.oracle, "the digest after the edit is not the files digest of the crate today");
    println!("edited: {} then {}", seen.before, seen.after);
}

/// `mac/src/lib.rs` touched, its bytes the same: the sweep reads that one file (its stamp moved,
/// so the stamp proves nothing), finds the same id, and the digest stands with no other read and
/// no move of the layout.
/// Prints `touched: read <n> bytes (mac/src/lib.rs is <n>), digest <hex>`.
#[test]
fn a_touched_member_macro_file_is_read_once_and_the_digest_stands() {
    let seen = run(Step::Touch);
    assert_eq!(seen.read, seen.lib_len, "a touch of mac/src/lib.rs: the sweep read {} bytes, not that one file's {}", seen.read, seen.lib_len);
    assert!(!seen.layout_moved, "a touch with the same bytes moved the layout");
    assert_eq!(seen.after, seen.before, "a touch with the same bytes moved the digest");
    println!("touched: read {} bytes (mac/src/lib.rs is {}), digest {}", seen.read, seen.lib_len, seen.after);
}

/// A file made in a folder of the crate is caught by that folder's stamp (rule 21): its
/// `MacroFolder` row is listed again and moves, and the digest after is the files digest today.
/// Prints `added: <before> then <after>`.
#[test]
fn a_file_added_to_a_member_macro_crate_gives_a_new_digest() {
    let seen = run(Step::Add);
    assert!(seen.layout_moved, "a file made in mac/src/expand did not move the layout");
    assert_ne!(seen.after, seen.before, "a file made in mac/src/expand left the crate's digest standing");
    assert_eq!(seen.after, seen.oracle, "the digest after the addition is not the files digest of the crate today");
    println!("added: {} then {}", seen.before, seen.after);
}
