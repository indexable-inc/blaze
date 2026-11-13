//! [`holds`] against a model plan over a real VFS: the model reads what a cargo plan reads of a
//! package (its manifest, whether `src/main.rs` and `src/lib.rs` are there, the listing of
//! `src/bin`, whose programs are its `.rs` files and its directories) through the same two kinds of read, records them as the engine does, and its value
//! is what it read. The oracle is a fresh model plan of the edited view.

use blaze_core::FileName;
use blaze_ids::ContentId;
use blaze_vfs::{Mode, Op, Vfs};
use hegel::generators as gs;

use super::{Plant, PlanReads, holds, PLANT};

/// What the model plan is made of, as a plan's units are of what it read.
#[derive(Debug, PartialEq, Eq)]
struct Model {
    manifest: Option<Vec<u8>>,
    main: bool,
    lib: bool,
    /// The programs under `src/bin`: each `.rs` file, and each directory (`name/`).
    bins: Vec<String>,
}

fn vfs() -> Vfs {
    Vfs::recover(FileName::new("state/journal").unwrap(), 0, &[]).0
}

fn write(path: &str, bytes: &[u8]) -> Op {
    Op::Write { mode: Mode::Regular, path: path.to_owned(), bytes: bytes.to_vec() }
}

/// The checkout every case starts from.
fn base(vfs: &mut Vfs) -> ContentId {
    let empty = vfs.empty();
    vfs.apply(
        empty,
        &[
            write("Cargo.toml", b"[package]\nname = \"app\"\n"),
            write("README", b"app\n"),
            write("src/main.rs", b"fn main() {}\n"),
            write("src/lib.rs", b"pub fn f() {}\n"),
            write("src/bin/tool.rs", b"fn main() {}\n"),
            write("src/bin/multi/main.rs", b"mod helper;\nfn main() {}\n"),
        ],
    )
    .unwrap()
}

/// The model plan of `root` and the reads it made, each at the id it saw (an absent file or
/// directory at an id of its own, as the engine fingerprints an absent input). As the real
/// plan's `Snap::kind`, whether a file is there is read from its directory's names, never from
/// the file, so a body edit is read by no step of it.
fn plan(vfs: &Vfs, root: ContentId) -> (Model, PlanReads) {
    let reader = vfs.reader();
    let found = reader.file(root, "Cargo.toml").unwrap();
    let id = found.as_ref().map_or(ContentId::of("blaze test: absent file", b"Cargo.toml"), |it| it.0);
    let manifest = found.map(|it| it.2.to_vec());
    let mut listings = Vec::new();
    let mut list = |path: &str| {
        let names: Vec<(String, bool)> = reader.list(root, path).map(Iterator::collect).unwrap_or_default();
        let mut listed = Vec::new();
        for (name, dir) in &names {
            listed.extend_from_slice(name.as_bytes());
            listed.push(if *dir { b'/' } else { b'\n' });
        }
        listings.push((Box::<str>::from(path), ContentId::of("blaze test: a listing", &listed)));
        names
    };
    let src = list("src");
    let main = src.iter().any(|(name, dir)| !dir && name == "main.rs");
    let lib = src.iter().any(|(name, dir)| !dir && name == "lib.rs");
    let bins = list("src/bin")
        .into_iter()
        .filter(|(name, dir)| *dir || name.ends_with(".rs"))
        .map(|(name, dir)| if dir { format!("{name}/") } else { name })
        .collect();
    let inputs = blaze_query::Inputs { files: vec![(Box::from("Cargo.toml"), id)], listings };
    (Model { manifest, main, lib, bins }, PlanReads::of(inputs).unwrap())
}

/// One edit a person makes, by kind; `n` names what it adds.
fn edit(kind: u8, n: u8) -> Op {
    match kind % KINDS {
        0 => write("src/main.rs", format!("fn main() {{ let _ = {n}; }}\n").as_bytes()),
        1 => write("src/lib.rs", format!("// {n}\npub fn f() {{}}\n").as_bytes()),
        2 => write("Cargo.toml", format!("[package]\nname = \"app\"\nversion = \"0.{n}.0\"\n").as_bytes()),
        3 => write(&format!("src/bin/new{n}.rs"), b"fn main() {}\n"),
        4 => Op::Delete { path: "src/bin/tool.rs".to_owned() },
        5 => write("README", format!("app {n}\n").as_bytes()),
        // A new program of its own directory: `src/bin` lists one more name.
        6 => write(&format!("src/bin/sub{n}/main.rs"), b"fn main() {}\n"),
        // A new module of a program's directory, which no step lists.
        _ => write(&format!("src/bin/multi/helper{n}.rs"), b"pub fn h() {}\n"),
    }
}

/// How many kinds of edit [`edit`] makes.
const KINDS: u8 = 8;

/// Whether an edit of `kind` is of what the plan reads nothing of: a body or a comment of a
/// source, a file no step reads (`README`), or a new file in a directory no step lists (a
/// program's own `src/bin/multi/`, under the listed `src/bin` and `src`: the listings above it are
/// as they were).
fn source_only(kind: u8) -> bool {
    matches!(kind % KINDS, 0 | 1 | 5 | 7)
}

/// The invariant: for any run of a person's edits of a checkout (a body, a comment, its
/// `Cargo.toml`, a new file or directory under `src/bin`, a removed file, a file nothing reads, a
/// new file of a directory nothing lists), a kept plan that [`holds`] over the difference of the
/// two views (its files and its relisted directories) equals a fresh plan of the edited one (it is
/// never kept wrongly), and edits of sources alone always keep it (a body edit plans nothing).
/// The control is [`the_oracle_catches_a_hold_blind_to_listings`].
#[hegel::test]
fn a_source_edit_keeps_the_plan_and_its_units(tc: hegel::TestCase) {
    let mut vfs = vfs();
    let before = base(&mut vfs);
    let (kept, reads) = plan(&vfs, before);
    let count = 1 + usize::from(tc.draw(gs::integers::<u8>())) % 4;
    let mut ops = Vec::new();
    let mut sources = true;
    for _ in 0..count {
        let (kind, n) = (tc.draw(gs::integers::<u8>()), tc.draw(gs::integers::<u8>()));
        sources &= source_only(kind);
        // A removal of what an earlier edit of the run removed is no edit.
        if kind % KINDS == 4 && ops.iter().any(|it| matches!(it, Op::Delete { .. })) {
            continue;
        }
        ops.push(edit(kind, n));
    }
    let after = vfs.apply(before, &ops).unwrap();
    let changed = vfs.diff(before, after);
    let relisted = vfs.relisted(before, after);
    let (fresh, _) = plan(&vfs, after);
    if holds(&reads, &changed, &relisted) {
        assert_eq!(fresh, kept, "kept over {changed:?} and {relisted:?}, which a fresh plan differs by");
    }
    if sources {
        assert!(holds(&reads, &changed, &relisted), "a source edit planned again: {changed:?}");
    }
}

/// The control: with the relisted directories left out ([`Plant::ListingBlind`]) a new file
/// under `src/bin`, and a new directory there (the tree keeps no empty directory, so a directory
/// is new with a file in it), each keep the plan, and a fresh plan of the edited view has one more
/// program, which the oracle above catches. The relisted directories are what a new directory
/// changes: its file's own path is under no listing the plan read.
#[test]
fn the_oracle_catches_a_hold_blind_to_listings() {
    for kind in [3, 6] {
        let mut vfs = vfs();
        let before = base(&mut vfs);
        let (kept, reads) = plan(&vfs, before);
        let after = vfs.apply(before, &[edit(kind, 7)]).unwrap();
        let changed = vfs.diff(before, after);
        let relisted = vfs.relisted(before, after);
        assert!(relisted.iter().any(|it| it == "src/bin"), "kind {kind}: {relisted:?}");
        assert!(!holds(&reads, &changed, &relisted), "kind {kind}: a new program kept the plan");
        PLANT.set(Plant::ListingBlind);
        let planted = holds(&reads, &changed, &relisted);
        PLANT.set(Plant::None);
        let (fresh, _) = plan(&vfs, after);
        assert!(
            planted && fresh != kept,
            "kind {kind}: the planted hold was not caught: kept {planted}, {fresh:?} against {kept:?}"
        );
    }
}
