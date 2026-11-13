//! The cutoffs of `items(file)` end to end, through the kernel (AGENTS.md rule 67, end-state.md
//! review points 1 and 4): a session holding `parse`, `items` and `lines` of one file of 200
//! functions and, for every function, a reader of its header ([`HeaderOf`]), of its body
//! ([`BodyOf`]) and of its line row ([`LineOf`]), and one reader of the items' key set
//! ([`KeysOf`]). Each test edits the file, carries the session with the exact change set (the
//! file's text differs, nothing else), and asks every reader again: a reader that is not a
//! session hit reran. Every reader's value is checked against [`crate::items::file_items`] of
//! the text as it is now, so a reader that should have rerun and did not fails.
//!
//! The grain of a part: an item's entry (its header) and its body are SEPARATE parts
//! ([`crate::entry::body_key`]), so a body edit and a header edit reach disjoint readers.
//!
//! Each test's doc gives the floor; the carry's own counts ([`Carried`]) are pinned beside the
//! readers'. What a carry costs in gets is the kernel's (`blaze-kernel/src/tests/cost.rs`).
//!
//! Over the floor, and owed: `items` and `lines` each walk the file (the floor is one walk:
//! `lines` could take the items and their ids from `items`' entries, whose children are in
//! source order, and count only positions).

use std::{cell::Cell, collections::BTreeMap, format, string::String, vec, vec::Vec};

use blaze_cas::{Key as StoreKey, Kind as StoreKind, Mismatch, content_of};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{
    Carried, Changed, CodeId, Compute, Get, Kernel, Pin, Put, Read, Reads, Seen, Session, Step,
    StepError, StepId, Subject, World, WorldError,
};
use blaze_store_check::Setting;
use syntax::{Edition, Extensions};

use crate::{
    entry::{Entry, ItemId, Kind, body_key},
    items::{FileItems, ITEMS},
    lines::LINES,
    parse::Reading,
    steps::{FileKey, Items, Lines, Parse, Source, reads},
    table::Table,
    tests::{id_of, items_of},
};

const NAME: &str = "src/lib.rs";
const HASHED: &str = "blaze v2 front cutoffs test 1";
/// What a reader of a part the table has not gives.
const GONE: &[u8] = b"gone";

/// The pinned tree: files by name, and how many reads it valued.
struct Tree {
    files: BTreeMap<String, String>,
    valued: Cell<usize>,
}

impl World for Tree {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        self.valued.set(self.valued.get() + 1);
        let name = core::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a key that is not UTF-8"))?;
        match read.kind {
            reads::FILE => Ok(self.files.get(name).map_or(Seen::Gone, |text| Seen::Is(ContentId::of(HASHED, text.as_bytes())))),
            reads::EXISTS => Ok(if self.files.contains_key(name) { Seen::Is(ContentId::of(HASHED, b"there")) } else { Seen::Gone }),
            reads::CFG => Ok(Seen::Is(ContentId::of(HASHED, &[u8::from(!name.ends_with("\0off"))]))),
            _ => Err(WorldError::new(read, "no such kind of read")),
        }
    }

    fn pin(&self) -> Pin {
        let mut hasher = Hasher::new(HASHED);
        for (name, text) in &self.files {
            hasher.part(name.as_bytes()).part(text.as_bytes());
        }
        Pin(hasher.finish())
    }
}

impl Source for Tree {
    fn file(&self, name: &str) -> Result<Option<&str>, WorldError> {
        Ok(self.files.get(name).map(String::as_str))
    }

    fn cfg(&self, _: &str, predicate: &str) -> Result<bool, WorldError> {
        Ok(predicate != "off")
    }
}

/// A store in memory, its gets counted.
#[derive(Default)]
struct Memory {
    values: BTreeMap<StoreKey, Vec<u8>>,
    gets: Cell<usize>,
}

impl Get for Memory {
    fn get(&self, key: &StoreKey) -> Result<Option<&[u8]>, String> {
        self.gets.set(self.gets.get() + 1);
        Ok(self.values.get(key).map(Vec::as_slice))
    }

    fn has(&self, key: &StoreKey) -> Result<bool, String> {
        Ok(self.values.contains_key(key))
    }
}

impl Put for Memory {
    fn put(&mut self, key: StoreKey, _: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) }),
            Some(_) => Ok(()),
            None => {
                self.values.insert(key, bytes);
                Ok(())
            }
        }
    }
}

/// One item of one file: what a reader of one item is asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ItemKey {
    file: FileKey,
    item: ItemId,
}

trait Subjected {
    fn subject_of(&self, what: &'static str) -> Subject;
}

impl Subjected for ItemKey {
    fn subject_of(&self, what: &'static str) -> Subject {
        let mut hasher = Hasher::new(what);
        hasher.part(self.file.name.as_bytes()).part(&self.item.0);
        Subject { of: hasher.finish(), flags: ContentId::default() }
    }
}

impl Subjected for FileKey {
    fn subject_of(&self, what: &'static str) -> Subject {
        Subject { of: ContentId::of(what, self.name.as_bytes()), flags: ContentId::default() }
    }
}

macro_rules! reader {
    ($(#[$doc:meta])* $name:ident, $id:literal, $text:literal, $input:ty) => {
        $(#[$doc])*
        struct $name;

        impl Step for $name {
            const ID: StepId = StepId($id);
            const NAME: &'static str = $text;
            const KIND: StoreKind = StoreKind::Query;
            type Input = $input;

            fn subject(input: &$input) -> Subject {
                input.subject_of($text)
            }

            fn label(input: &$input) -> String {
                format!("{} {input:?}", $text)
            }
        }
    };
}

reader!(
    /// The header hash of one item: one part of `items(file)`, the item's entry.
    HeaderOf, 0x7f61, "cutoffs header of", ItemKey
);
reader!(
    /// The body of one item: one part of `items(file)`, under the body's key.
    BodyOf, 0x7f62, "cutoffs body of", ItemKey
);
reader!(
    /// The line row of one item: one part of `lines(file)`.
    LineOf, 0x7f63, "cutoffs line of", ItemKey
);
reader!(
    /// How many keys `items(file)` has: a reader of its key set.
    KeysOf, 0x7f64, "cutoffs keys of", FileKey
);

impl Compute for HeaderOf {
    type World = Tree;

    fn compute(ctx: &mut Reads<'_, Tree>, input: &ItemKey) -> Result<Vec<u8>, StepError> {
        match ctx.part::<Items<Tree>>(&input.file, &input.item.0)? {
            Some(bytes) => Ok(Entry::read(&bytes).map_err(StepError::Refused)?.header.to_vec()),
            None => Ok(GONE.to_vec()),
        }
    }
}

impl Compute for BodyOf {
    type World = Tree;

    fn compute(ctx: &mut Reads<'_, Tree>, input: &ItemKey) -> Result<Vec<u8>, StepError> {
        Ok(ctx.part::<Items<Tree>>(&input.file, &body_key(input.item))?.unwrap_or_else(|| GONE.to_vec()))
    }
}

impl Compute for LineOf {
    type World = Tree;

    fn compute(ctx: &mut Reads<'_, Tree>, input: &ItemKey) -> Result<Vec<u8>, StepError> {
        Ok(ctx.part::<Lines<Tree>>(&input.file, &input.item.0)?.unwrap_or_else(|| GONE.to_vec()))
    }
}

impl Compute for KeysOf {
    type World = Tree;

    fn compute(ctx: &mut Reads<'_, Tree>, input: &FileKey) -> Result<Vec<u8>, StepError> {
        Ok((ctx.keys::<Items<Tree>>(input)?.len() as u32).to_le_bytes().to_vec())
    }
}

/// What each reader should give, from the file's items as they are now.
fn header_now(items: &FileItems, id: ItemId) -> Vec<u8> {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    let entry = table.get(&id.0).expect("a whole table");
    entry.map_or(GONE.to_vec(), |bytes| Entry::read(bytes).expect("an entry").header.to_vec())
}

fn body_now(items: &FileItems, id: ItemId) -> Vec<u8> {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    table.get(&body_key(id)).expect("a whole table").map_or(GONE.to_vec(), <[u8]>::to_vec)
}

fn line_now(items: &FileItems, id: ItemId) -> Vec<u8> {
    let table = Table::read(LINES, &items.lines).expect("a line table");
    table.get(&id.0).expect("a whole table").map_or(GONE.to_vec(), <[u8]>::to_vec)
}

fn keys_now(items: &FileItems) -> Vec<u8> {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    (table.len() as u32).to_le_bytes().to_vec()
}

/// The 200 functions, each with a header and a body.
fn two_hundred() -> String {
    let mut text = String::new();
    for n in 0..200 {
        text.push_str(&format!("pub fn f{n}(x: u32) -> u32 {{\n    x + {n}\n}}\n\n"));
    }
    text
}

/// The session, and who is in it.
struct Driven {
    kernel: Kernel<Tree>,
    store: Memory,
    world: Tree,
    session: Session,
    file: FileKey,
    /// The functions' ids, `f0` to `f199`.
    ids: Vec<ItemId>,
}

/// Which readers were not session hits after a carry, by function number.
#[derive(Debug, PartialEq, Eq, Default)]
struct Reran {
    header: Vec<usize>,
    body: Vec<usize>,
    line: Vec<usize>,
    keys: bool,
}

fn driven() -> Driven {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Parse<Tree>>(CodeId(ContentId([61; 32]))).unwrap();
    kernel.register_ask::<Items<Tree>>(CodeId(ContentId([62; 32]))).unwrap();
    kernel.register_ask::<Lines<Tree>>(CodeId(ContentId([63; 32]))).unwrap();
    kernel.register::<HeaderOf>(CodeId(ContentId([64; 32]))).unwrap();
    kernel.register::<BodyOf>(CodeId(ContentId([65; 32]))).unwrap();
    kernel.register::<LineOf>(CodeId(ContentId([66; 32]))).unwrap();
    kernel.register::<KeysOf>(CodeId(ContentId([67; 32]))).unwrap();
    let text = two_hundred();
    let items = items_of(&text);
    let ids = (0..200).map(|n| id_of(&items, Kind::Fn, &format!("f{n}"))).collect();
    let world = Tree { files: BTreeMap::from([(String::from(NAME), text)]), valued: Cell::new(0) };
    let session = Session::new(&world);
    let file = FileKey { name: String::from(NAME), reading: Reading { edition: Edition::CURRENT, extensions: Extensions::NONE } };
    let mut it = Driven { kernel, store: Memory::default(), world, session, file, ids };
    let first = it.ask();
    assert_eq!(first.header.len(), 200, "every reader ran once, into the session");
    assert!(first.keys);
    it
}

impl Driven {
    /// Writes `text` as the file and carries the session with the exact change: the file's
    /// text differs, no file came or went, no cfg changed.
    fn edit(&mut self, text: String) -> Carried {
        let from = self.world.pin();
        self.world.files.insert(String::from(NAME), text);
        let mut changed = Changed::between(from, self.world.pin());
        changed.differs(Read { kind: reads::FILE, key: NAME.as_bytes() });
        changed.exact(reads::EXISTS);
        changed.exact(reads::CFG);
        self.kernel.carry(&mut self.session, &mut self.store, &self.world, &changed, &Setting::OFF).unwrap()
    }

    /// Asks every reader in the session: which were not hits. Each one's value is checked
    /// against the items of the text as it is now.
    fn ask(&mut self) -> Reran {
        let now = items_of(&self.world.files[NAME]);
        let mut reran = Reran::default();
        let Driven { kernel, store, world, session, file, ids } = self;
        for (n, id) in ids.iter().enumerate() {
            let input = ItemKey { file: file.clone(), item: *id };
            let got = kernel.get_in::<HeaderOf, _>(session, store, world, &input, &Setting::OFF).unwrap();
            assert_eq!(got.value(), header_now(&now, *id), "the header reader of f{n}");
            if !got.is_hit() {
                reran.header.push(n);
            }
            let got = kernel.get_in::<BodyOf, _>(session, store, world, &input, &Setting::OFF).unwrap();
            assert_eq!(got.value(), body_now(&now, *id), "the body reader of f{n}");
            if !got.is_hit() {
                reran.body.push(n);
            }
            let got = kernel.get_in::<LineOf, _>(session, store, world, &input, &Setting::OFF).unwrap();
            assert_eq!(got.value(), line_now(&now, *id), "the line reader of f{n}");
            if !got.is_hit() {
                reran.line.push(n);
            }
        }
        let got = kernel.get_in::<KeysOf, _>(session, store, world, file, &Setting::OFF).unwrap();
        assert_eq!(got.value(), keys_now(&now), "the key-set reader");
        reran.keys = !got.is_hit();
        reran
    }
}

/// What the carry of one edit of the file does: `parse`, `items` and `lines` each read the
/// file's text, so the change set reaches the three (seeded) and each is looked up again
/// (repaired); `changed` of them are other bytes and `cut` the same; `gone` readers are
/// dropped, each a reader of a part that differs, which reruns when it is next asked for.
fn carried(changed: usize, cut: usize, gone: usize) -> Carried {
    Carried { seeded: 3, repaired: 3, changed, cut, gone, ..Carried::default() }
}

/// (a) A comment edit. The floor: `parse` and `items` rerun, `items`' value is the same
/// bytes, so the carry cuts there and no reader of an item's entry or body is reached; `lines`
/// reruns and changes, and reaches the readers of the rows that moved: none for a comment
/// after the last item, every function's for a comment above the first.
#[test]
fn a_comment_edit_reaches_no_reader_of_an_item() {
    let mut it = driven();
    let below = format!("{}// a comment after every item\n", two_hundred());
    assert_eq!(it.edit(below), carried(2, 1, 0), "parse and lines changed, items cut, no row moved");
    assert_eq!(it.ask(), Reran::default());

    let above = format!("// a comment above every item\n{}// a comment after every item\n", two_hundred());
    assert_eq!(it.edit(above), carried(2, 1, 200), "every row moved: its 200 readers, no other");
    assert_eq!(it.ask(), Reran { line: (0..200).collect(), ..Reran::default() });
}

/// (b) A body edit of one function, its length kept so no row moves. The floor: exactly that
/// body's reader; no header reader, no other body's, no line reader (the rows are the same
/// bytes, so `lines` is cut too).
#[test]
fn a_body_edit_reaches_exactly_that_bodys_reader() {
    let mut it = driven();
    let edited = two_hundred().replace("    x + 3\n", "    x * 3\n");
    assert_eq!(it.edit(edited), carried(2, 1, 1), "parse and items changed, lines cut");
    assert_eq!(it.ask(), Reran { body: vec![3], ..Reran::default() });
}

/// (c) A header edit of one function, its length kept. The floor: exactly that header's
/// reader. Its body's reader is not reached: the body is a part of its own, and holds no
/// position (the body's token span is in the line row).
///
/// `parse` is cut here too: its record is the tree's shape (kinds and lengths) and holds no
/// text, and `u32` to `u64` is one identifier for another of the same length. `items` is not
/// cut by it: it reads the file's text itself, so it reruns and its header part changes.
#[test]
fn a_header_edit_reaches_exactly_that_headers_reader() {
    let mut it = driven();
    let edited = two_hundred().replace("pub fn f3(x: u32) -> u32 {", "pub fn f3(x: u64) -> u32 {");
    assert_eq!(it.edit(edited), carried(1, 2, 1), "items changed; parse (same shape) and lines cut");
    assert_eq!(it.ask(), Reran { header: vec![3], ..Reran::default() });
}

/// (d) An item inserted between `f99` and `f100`. The floor: no reader of an existing item's
/// entry or body (no id moved: an id is a hash of what names the item, never an ordinal);
/// the reader of the key set, which a new key reaches; and the readers of the rows that moved
/// (`f100` to `f199`, below the insert). The file's own entry changed (its children) and
/// nobody here reads it.
#[test]
fn an_inserted_item_reaches_only_the_key_set_and_the_rows_below() {
    let mut it = driven();
    let edited = two_hundred().replace("pub fn f100(", "pub fn inserted(x: u32) -> u32 {\n    x\n}\n\npub fn f100(");
    assert_eq!(it.edit(edited), carried(3, 0, 101), "parse, items and lines changed");
    assert_eq!(it.ask(), Reran { line: (100..200).collect(), keys: true, ..Reran::default() });
}
