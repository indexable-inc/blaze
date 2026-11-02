//! A source file's [`ItemTree`], made from the file's last tree where the file was edited.
//!
//! One query per file ([`super::file_item_tree_query`]) and no query per item. Beside the query database,
//! a few files' last trees are kept ([`KEPT`]), with the text they were made of and a key per
//! top-level item: all that lowering the item reads ([`item_keys`]: a fingerprint of its
//! syntax but for its bodies, its ast ids and its span anchors) and where its rows are in the
//! tree ([`Marks`]). An edit keys the new parse's items, hashing only those whose text it
//! touched ([`Unchanged`]) and reading ids and anchors from the maps only for those in which
//! an entry of the maps changed ([`Unchanged::touched`]), keeps the rows of the unchanged items
//! before and after the changed ones (copied, their ids moved), lowers the changed ones alone
//! and assembles the file's tree from them ([`assemble`]). An edit inside a top-level `fn`'s
//! body, or an
//! `impl`'s or `trait`'s, changes no key and returns the last tree itself. A file's first
//! lowering is [`super::lower_whole`] and makes no keys, so a file never edited never pays for
//! them; its first edit lowers it whole again, keying it. The tree is the one `lower_whole`
//! makes, to the byte, however it was made.
//!
//! A slot is one database's file in one crate ([`Kept`]). One another file took, or one
//! another thread holds, is a whole lowering, never a wrong one: what decides reuse is the
//! keys, the crate's `cfg` set and the top level's attributes: keyed ([`top_key`]), and
//! lowered again and compared where the key changed.

use std::{
    cell::Cell,
    cmp::Ordering,
    hash::{Hash, Hasher},
    mem,
    sync::Arc,
};

use base_db::{Crate, Db, EditionedFileId, Nonce};
use blaze_ids::Domain;
use cfg::CfgOptions;
use hir_expand::{HirFileId, span_map::SpanMap};
use parking_lot::Mutex;
use span::{AstIdMap, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, RealSpanMap};
use syntax::{
    NodeOrToken, SyntaxKind, Text, TextRange, TextSize, ast as tag,
    view::{self as ast, AstNode, HasModuleItem},
};

use crate::item_tree::{
    AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, ItemKind,
    ItemTree, ItemTreeLens, ItemTreeSinks, Macro2, MacroCall, MacroRules, Mod, ModItemId, NameId,
    PathId, RawVisibilityId, Static, Struct, Trait, TypeAlias, Union, Use, UseTree, UseTreeId,
    lower,
};

#[cfg(test)]
mod tests;

/// The tree of a source file: the kept tree of its last lowering, or one assembled from its
/// rows and the changed items', or a whole lowering ([`self`]).
pub(super) fn file_item_tree(db: &Db, file: EditionedFileId, krate: Crate) -> Arc<ItemTree> {
    let db_nonce = db.nonce_and_revision().0;
    let kept =
        with_slot(db_nonce, file, krate, |it| it.lock().take());
    let kept = kept.filter(|it| (it.db, it.file, it.krate) == (db_nonce, file, krate));
    // Read on every path, so the query depends on it even where nothing lowered reads it.
    let cfg = krate.cfg_options(db);
    let (tree, keyed) = match kept {
        None => {
            count_lowered(|| file.parse(db).view().items().count());
            // alloc: the tree's handle, once a lowering
            (Arc::new(super::lower_whole(db, file.into(), krate)), None)
        }
        Some(Kept { keyed: Some(keyed), .. }) if keyed.cfg == *cfg => {
            let keyed = reuse(db, file, krate, keyed);
            (keyed.tree.clone(), Some(keyed)) // clone: an `Arc`, the tree the slot keeps
        }
        Some(Kept { keyed, .. }) => {
            let keyed = lower_keyed(db, file, krate, cfg, keyed);
            (keyed.tree.clone(), Some(keyed)) // clone: an `Arc`, the tree the slot keeps
        }
    };
    let replaced = with_slot(db_nonce, file, krate, |it| {
        (it.lock())
            .replace(Kept { db: db_nonce, file, krate, keyed })
    });
    // Dropped after the lock: a tree let go of may be a whole file's columns.
    drop(replaced);
    tree
}

/// How many files' last trees are kept: the files being edited, and the last few others.
const SLOTS: usize = 128;

type Slot = Mutex<Option<Kept>>;

/// The files' last trees, direct-mapped by database, file and crate: a lookup is one hash, one
/// index and one short lock, and the slot is empty while its tree is worked from.
#[cfg(not(test))]
static KEPT: [Slot; SLOTS] = [const { Mutex::new(None) }; SLOTS];

#[cfg(test)]
thread_local! {
    /// A test thread's own slots: a test that counts what an edit lowers sees no other test's
    /// file take its slot between its edits.
    static KEPT: [Slot; SLOTS] = const { [const { Mutex::new(None) }; SLOTS] };
}

fn with_slot<R>(db: Nonce, file: EditionedFileId, krate: Crate, f: impl FnOnce(&Slot) -> R) -> R {
    let mut hasher = blaze_hash::FixedHasher::default();
    (db, file, krate).hash(&mut hasher);
    // The multiply leaves its best bits at the top.
    let at = (hasher.finish() >> (64 - SLOTS.trailing_zeros())) as usize;
    #[cfg(not(test))]
    return f(&KEPT[at]);
    #[cfg(test)]
    return KEPT.with(|kept| f(&kept[at]));
}

/// A file's last lowering in a crate, of a database: the ids are that database's.
struct Kept {
    db: Nonce,
    file: EditionedFileId,
    krate: Crate,
    /// None after a first lowering, which makes no keys.
    keyed: Option<Keyed>,
}

/// A file's last tree and what its rows were lowered from.
struct Keyed {
    /// The crate's `cfg` set the tree was lowered under: lowering an item's attributes reads it.
    cfg: CfgOptions,
    tree: Arc<ItemTree>,
    /// The file's text the keys were made of: what the next edit's text is compared with.
    text: Text,
    /// What lowering the top level's attributes read ([`top_key`]).
    top: u128,
    /// Per top-level item of the file, in order: what lowering it reads.
    keys: Keys,
    /// Per top-level item, where its rows begin in each of `tree`'s columns; then where the
    /// last item's end. One more than `keys.items`.
    marks: Vec<Marks>,
    /// The file's ast id map's entries ([`AstIdMap::entries`]) and span map's anchors as
    /// `[start, id]`, as `keys` was made: the next edit compares the new maps' with these in the
    /// text it left unchanged, so an item no entry or anchor of which changed copies its ids and
    /// anchors from `keys` ([`Unchanged::touched`]).
    entries: Vec<[u32; 4]>,
    anchors: Vec<[u32; 2]>,
}

/// What lowering reads of each top-level item of a file ([`item_keys`]): its syntax, as a
/// fingerprint, and the ast ids and span anchors within it, as they are, all items' in two
/// arrays one after the other.
#[derive(Default)]
struct Keys {
    items: Vec<ItemKey>,
    ids: Vec<[u32; 3]>,
    anchors: Vec<[u32; 2]>,
}

#[derive(Clone, Copy)]
struct ItemKey {
    /// Where the item is in the text it was keyed in.
    range: TextRange,
    /// The fingerprint of the item's syntax but for what lowering does not read ([`hash_item`]).
    syntax: u128,
    /// Where the item's ids end in [`Keys::ids`], and its anchors in [`Keys::anchors`].
    ids_end: u32,
    anchors_end: u32,
}

impl Keys {
    fn clear(&mut self) {
        self.items.clear();
        self.ids.clear();
        self.anchors.clear();
    }

    fn ids(&self, item: usize) -> &[[u32; 3]] {
        let from = item.checked_sub(1).map_or(0, |it| self.items[it].ids_end);
        &self.ids[from as usize..self.items[item].ids_end as usize]
    }

    fn anchors(&self, item: usize) -> &[[u32; 2]] {
        let from = item.checked_sub(1).map_or(0, |it| self.items[it].anchors_end);
        &self.anchors[from as usize..self.items[item].anchors_end as usize]
    }

    /// Whether lowering `item` of these keys reads what lowering `theirs` of `other` does.
    fn same(&self, item: usize, other: &Keys, theirs: usize) -> bool {
        self.items[item].syntax == other.items[theirs].syntax
            && self.ids(item) == other.ids(theirs)
            && self.anchors(item) == other.anchors(theirs)
    }
}

/// The text of `source_file`'s tree: the file's, shared.
fn text_of<'t>(source_file: ast::SourceFile<'t>) -> &'t Text {
    source_file.syntax().file().source()
}

/// The tree of an edited file from its kept one: the rows of the items whose keys are the
/// same before and after the edit's, kept, and the items between lowered again.
fn reuse(db: &Db, file: EditionedFileId, krate: Crate, mut keyed: Keyed) -> Keyed {
    let source_file = file.parse(db).view();
    let mut scratch = SCRATCH.take();
    scratch.keys.clear();
    item_keys(db, file, source_file, &mut scratch, Some(&keyed));
    keyed.text = text_of(source_file).clone(); // clone: one count of the file's text
    mem::swap(&mut keyed.entries, &mut scratch.entries);
    mem::swap(&mut keyed.anchors, &mut scratch.anchors);
    let (old, new) = (&keyed.keys, &scratch.keys);
    let (old_items, new_items) = (old.items.len(), new.items.len());
    let before = (0..old_items.min(new_items)).take_while(|&it| old.same(it, new, it)).count();
    let after = (0..(old_items - before).min(new_items - before))
        .take_while(|&it| old.same(old_items - 1 - it, new, new_items - 1 - it))
        .count();
    let top_key = top_key(db, file, krate, source_file, &mut scratch.bytes);
    let top_kept = mem::replace(&mut keyed.top, top_key) == top_key;
    let unchanged = before == old_items && before == new_items;
    let top = (!unchanged || !top_kept).then(|| top_attrs(db, file, krate));
    let top = match top {
        Some(top) if !unchanged || !same_top(&keyed.tree, &keyed.marks[0], &top) => top,
        _ => {
            // The tree and its marks stand; the items' ranges moved with the edit.
            mem::swap(&mut keyed.keys, &mut scratch.keys);
            release(scratch);
            return keyed;
        }
    };

    let changed = new_items - before - after;
    count_lowered(|| changed);
    let fresh: Vec<ItemTree> = (source_file.items().skip(before).take(changed))
        .map(|item| lower::Ctx::new(db, file.into(), krate).lower_item(item))
        .collect(); // alloc: once an edit, the list of the items it changed
    let fresh_marks: Vec<[Marks; 2]> =
        fresh.iter().map(|it| [Marks::default(), Marks::ends(it)]).collect(); // alloc: as `fresh`
    let tree = {
        let (kept, marks) = (&*keyed.tree, &keyed.marks[..]);
        // alloc: once an edit, the list of its parts
        let mut parts = Vec::with_capacity(fresh.len() + 2);
        if before > 0 {
            parts.push(Rows { tree: kept, marks: &marks[..=before] });
        }
        parts.extend(fresh.iter().zip(&fresh_marks).map(|(tree, marks)| Rows { tree, marks }));
        if after > 0 {
            parts.push(Rows { tree: kept, marks: &marks[old_items - after..] });
        }
        scratch.marks.clear();
        assemble(&top, &parts, &mut scratch.marks)
    };
    keyed.tree = Arc::new(tree); // alloc: the tree's handle, once an edit that changed it
    mem::swap(&mut keyed.keys, &mut scratch.keys);
    mem::swap(&mut keyed.marks, &mut scratch.marks);
    release(scratch);
    keyed
}

/// Whether `top`, the top level's attributes lowered again, are those `old` was made with:
/// `old`'s first attributes and `cfg`s, up to where its first item's (`first`) begin, and its
/// top level's owner.
fn same_top(old: &ItemTree, first: &Marks, top: &ItemTree) -> bool {
    let top_owner = |tree: &ItemTree| {
        tree.attr_owners().last().copied().filter(|it| it.owner == ModItemId::TOP_OWNER)
    };
    let (attrs, cfgs) = (top.attr_list().len(), top.cfgs().len());
    first.attr_list as usize == attrs
        && first.cfgs as usize == cfgs
        && old.attr_list()[..attrs] == top.attr_list()[..]
        && old.cfgs()[..cfgs] == top.cfgs()[..]
        && top_owner(old) == top_owner(top)
}

/// The tree of a file lowered whole, its items keyed: a file's first edit, or the first under
/// a new `cfg` set. `last` lends its arrays.
fn lower_keyed(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
    cfg: &CfgOptions,
    last: Option<Keyed>,
) -> Keyed {
    let source_file = file.parse(db).view();
    let mut scratch = SCRATCH.take();
    if let Some(last) = last {
        (scratch.keys, scratch.marks) = (last.keys, last.marks);
        (scratch.entries, scratch.anchors) = (last.entries, last.anchors);
    }
    scratch.keys.clear();
    scratch.marks.clear();
    item_keys(db, file, source_file, &mut scratch, None);
    count_lowered(|| scratch.keys.items.len());
    let top = top_key(db, file, krate, source_file, &mut scratch.bytes);

    let mut ctx = lower::Ctx::new(db, file.into(), krate);
    ctx.lower_top_attrs(source_file, crate_attrs_of(db, file, krate));
    let tree = ctx.lower_module_items_marked(source_file, &mut scratch.marks);
    let keyed = Keyed {
        cfg: cfg.clone(), // clone: once a file's first edit or a new `cfg` set
        tree: Arc::new(tree), // alloc: the tree's handle, once a lowering
        text: text_of(source_file).clone(), // clone: one count of the file's text
        top,
        keys: mem::take(&mut scratch.keys),
        marks: mem::take(&mut scratch.marks),
        entries: mem::take(&mut scratch.entries),
        anchors: mem::take(&mut scratch.anchors),
    };
    release(scratch);
    keyed
}

/// The tree of `file`'s top level's attributes alone: its inner attributes and, for a crate's
/// root, the crate attributes given on the command line.
pub(super) fn top_attrs(db: &Db, file: EditionedFileId, krate: Crate) -> ItemTree {
    let source_file = file.parse(db).view();
    lower::Ctx::new(db, HirFileId::from(file), krate)
        .lower_top(source_file, crate_attrs_of(db, file, krate))
}

/// A fingerprint of what lowering the top level's attributes reads ([`top_attrs`]) but the
/// crate's `cfg` set: the file's syntax outside its top-level items, at its offsets, and the
/// crate attributes' files. Where the items' keys are as they were, the same offsets put them
/// where they were, so the attributes' spans are too. One that changed may still lower to the
/// same attributes ([`same_top`]).
fn top_key(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
    source_file: ast::SourceFile<'_>,
    bytes: &mut Vec<u8>,
) -> u128 {
    bytes.clear();
    let mut feed = Feed(bytes);
    for child in source_file.syntax().children_with_tokens() {
        match child {
            NodeOrToken::Node(node) if ast::Item::can_cast(node.kind()) => {}
            NodeOrToken::Node(node) => {
                u32::from(node.text_range().start()).hash(&mut feed);
                hash_full(node, &mut feed);
            }
            NodeOrToken::Token(token) => {
                u32::from(token.text_range().start()).hash(&mut feed);
                hash_token(token, &mut feed);
            }
        }
    }
    for &attr_file in crate_attrs_of(db, file, krate) {
        (END, attr_file).hash(&mut feed);
        hash_full(attr_file.parse(db).view().syntax(), &mut feed);
    }
    feed.fingerprint()
}

/// The files of the crate attributes rustc appends to `file`'s inner attributes: `krate`'s
/// applied ones when `file` is its root, else none.
fn crate_attrs_of(db: &Db, file: EditionedFileId, krate: Crate) -> &[EditionedFileId] {
    match file == krate.root_file_id(db) {
        true => base_db::applied_crate_attr_files(db, krate),
        false => &[],
    }
}

/// The tree of one top-level item of `file` alone, as an edit lowers a changed item.
#[cfg(test)]
pub(super) fn item_alone(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
    item: ast::Item<'_>,
) -> ItemTree {
    lower::Ctx::new(db, file.into(), krate).lower_item(item)
}

#[cfg(test)]
thread_local! {
    /// How many top-level items this thread lowered, whole or alone, for the tests that count
    /// what an edit lowers.
    pub(super) static LOWERED: Cell<usize> = const { Cell::new(0) };
    /// How many top-level items' syntax this thread hashed ([`item_keys`]).
    pub(super) static HASHED: Cell<usize> = const { Cell::new(0) };
    /// How many top-level items' ast ids and anchors this thread read from the maps
    /// ([`item_keys`]), not copied from the last keys.
    pub(super) static READ: Cell<usize> = const { Cell::new(0) };
}

fn count_lowered(items: impl FnOnce() -> usize) {
    #[cfg(test)]
    LOWERED.set(LOWERED.get() + items());
    #[cfg(not(test))]
    let _ = items;
}

fn count_hashed() {
    #[cfg(test)]
    HASHED.set(HASHED.get() + 1);
}

fn count_read() {
    #[cfg(test)]
    READ.set(READ.get() + 1);
}

/// The domain of an item's key.
static ITEM: Domain = Domain::new("blaze v2 item syntax 2");

/// What [`Hash`] writes of one item, collected to hash in one BLAKE3 pass: an item's syntax is
/// tens of small writes, each a copy here where it would be an update of the hasher's state.
struct Feed<'a>(&'a mut Vec<u8>);

impl Feed<'_> {
    fn fingerprint(&self) -> u128 {
        blaze_ids::Hasher::of(&ITEM).update(&self.0[..]).finish().fingerprint().0
    }
}

impl Hasher for Feed<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        self.fingerprint() as u64
    }
}

/// The arrays [`item_keys`] and [`reuse`] fill, empty between runs and keeping their capacity,
/// so a worker grows none again once it has keyed a file as big; an edit swaps its keys and
/// marks with the slot's, so the slot's last arrays are the next edit's. A scratch that grew
/// past [`SCRATCH_IDS`] (a generated file) is dropped, so a worker keeps a bounded amount.
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    keys: Keys,
    marks: Vec<Marks>,
    /// The maps' entries and anchors keyed from, as [`Keyed`]'s.
    entries: Vec<[u32; 4]>,
    anchors: Vec<[u32; 2]>,
    /// One bit per new top-level item: whether an entry or anchor in it changed.
    touched: Vec<u64>,
}

thread_local! {
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// About 200 KiB of each of a [`Scratch`]'s arrays.
const SCRATCH_IDS: usize = 1 << 14;

fn release(mut scratch: Scratch) {
    scratch.keys.clear();
    scratch.marks.clear();
    scratch.entries.clear();
    scratch.anchors.clear();
    let keys = &scratch.keys;
    if keys.ids.capacity() <= SCRATCH_IDS
        && keys.items.capacity() <= SCRATCH_IDS
        && scratch.entries.capacity() <= SCRATCH_IDS
        && scratch.anchors.capacity() <= SCRATCH_IDS
    {
        SCRATCH.set(scratch);
    }
}

/// Pushes onto `scratch.keys` what lowering reads of each of `source_file`'s top-level items,
/// in order: the fingerprint of the item's syntax but for what lowering does not read
/// ([`hash_item`]), the ast ids of the items it is made of and the span anchors within it, all
/// from where it starts, so an item keeps its key wherever it moves in the file. An item whose
/// text `last`'s also had, unchanged ([`Unchanged`]), takes the fingerprint it had there, so an
/// edit hashes only the items it touched; and, when no entry of the ast id map and no anchor of
/// the span map in it changed ([`Unchanged::touched`]), its ids and anchors, so an edit reads
/// the maps per item only for the items it touched. Two walks of the items and one of each
/// map's arrays, which allocate nothing once the scratch is grown. Leaves the maps' arrays in
/// `scratch.entries` and `scratch.anchors`, for the next edit.
fn item_keys(
    db: &Db,
    file: EditionedFileId,
    source_file: ast::SourceFile<'_>,
    scratch: &mut Scratch,
    last: Option<&Keyed>,
) {
    let hir_file = HirFileId::from(file);
    let ast_id_map = hir_file.ast_id_map(db);
    let SpanMap::RealSpanMap(span_map) = hir_file.span_map(db) else {
        unreachable!("a source file's span map is a real file's")
    };
    let (pairs, starts) = span_map.anchors();
    let Scratch { bytes, keys, entries, anchors, touched, .. } = scratch;
    for item in source_file.items() {
        let range = item.syntax().text_range();
        keys.items.push(ItemKey { range, syntax: 0, ids_end: 0, anchors_end: 0 });
    }
    let unchanged = last.map(|last| Unchanged::new(last, text_of(source_file), &keys.items));
    touched.clear();
    touched.resize(keys.items.len().div_ceil(64), 0);
    if let Some((last, unchanged)) = last.zip(unchanged.as_ref()) {
        unchanged.touched(last, ast_id_map.entries(), pairs, starts, &keys.items, touched);
    }
    for (at, item) in source_file.items().enumerate() {
        let range = keys.items[at].range;
        let was = last.zip(unchanged.as_ref()).and_then(|(last, unchanged)| {
            unchanged.kept(&last.keys.items, &keys.items, at).map(|was| (last, was))
        });
        match was.filter(|_| touched[at / 64] & (1 << (at % 64)) == 0) {
            Some((last, was)) => {
                keys.ids.extend_from_slice(last.keys.ids(was));
                keys.anchors.extend_from_slice(last.keys.anchors(was));
            }
            None => {
                count_read();
                let ids_from = keys.ids.len();
                item_ids(item, ast_id_map, range.start(), &mut keys.ids);
                item_anchors(span_map, range, &keys.ids[ids_from..], &mut keys.anchors);
            }
        }
        let syntax = match was {
            Some((last, was)) => last.keys.items[was].syntax,
            None => {
                count_hashed();
                bytes.clear();
                let mut feed = Feed(&mut *bytes);
                hash_item(item.syntax(), &mut feed, hash_inner_attrs);
                feed.fingerprint()
            }
        };
        keys.items[at] =
            ItemKey { range, syntax, ids_end: id(keys.ids.len()), anchors_end: id(keys.anchors.len()) };
    }
    entries.clear();
    entries.extend_from_slice(ast_id_map.entries());
    anchors.clear();
    anchors.extend(starts.iter().zip(pairs).map(|(&start, &[ast_id, _])| [start, ast_id]));
}

fn id(len: usize) -> u32 {
    u32::try_from(len).expect("a file of more than `u32` ast ids")
}

/// What an edit left of a file's text: the longest prefix and suffix its text before and after
/// share. The parse of a file is a function of its text ([`syntax::Parse::reparse_to`]), and
/// the parser reads a top-level item from the item list's state, its tokens and at most three
/// tokens after the one it is at; the lexer starts a token at an item's start the same way
/// whatever came before. So an item with the same text at the same place in the suffix, which
/// runs to the end, is the same syntax; and so is one in the prefix with two whole items after
/// it there, which hold the three tokens the parser may look at past it.
struct Unchanged {
    /// The new parse's leading items that are in the prefix with two items after them.
    prefix_items: usize,
    /// The length of the prefix.
    prefix: u32,
    /// Where the suffix begins in the new text.
    suffix_from: TextSize,
    /// The old text's length less the new's: where an item of the suffix was, from where it is.
    shift: i64,
}

impl Unchanged {
    fn new(last: &Keyed, text: &Text, items: &[ItemKey]) -> Unchanged {
        let (old, new) = (last.text.as_bytes(), text.as_bytes());
        let (prefix, suffix) = common_ends(old, new);
        let in_prefix = items.iter().take_while(|it| usize::from(it.range.end()) <= prefix).count();
        Unchanged {
            prefix_items: in_prefix.saturating_sub(2),
            prefix: id(prefix),
            suffix_from: TextSize::new(id(new.len() - suffix)),
            shift: old.len() as i64 - new.len() as i64,
        }
    }

    /// The index in `old` of item `at` of `new`, if the item is one this edit left unchanged.
    fn kept(&self, old: &[ItemKey], new: &[ItemKey], at: usize) -> Option<usize> {
        let range = new[at].range;
        let (was, start) = if at < self.prefix_items {
            (at, i64::from(u32::from(range.start())))
        } else if range.start() >= self.suffix_from {
            let from_end = new.len() - at;
            (old.len().checked_sub(from_end)?, i64::from(u32::from(range.start())) + self.shift)
        } else {
            return None;
        };
        let it = old.get(was)?;
        let same = i64::from(u32::from(it.range.start())) == start && it.range.len() == range.len();
        same.then_some(was)
    }

    /// Whether what spans `start..end` of the new text is in the text this edit left unchanged.
    fn keeps(&self, start: u32, end: u32) -> bool {
        end <= self.prefix || start >= u32::from(self.suffix_from)
    }

    /// Where what spanned `start..end` of the last text is in the new, if it is in the text
    /// this edit left unchanged.
    fn moved(&self, start: u32, end: u32) -> Option<[u32; 2]> {
        if end <= self.prefix {
            return Some([start, end]);
        }
        let now = |it: u32| u32::try_from(i64::from(it) - self.shift).ok();
        let start = now(start).filter(|&it| it >= u32::from(self.suffix_from))?;
        Some([start, now(end)?])
    }

    /// Sets the bit in `touched` of each item of `items` (the new parse's) holding an entry of
    /// the ast id map or an anchor of the span map, in the text this edit left unchanged, that
    /// is not as it was in `last`'s maps; `entries`, `pairs` and `starts` are the new maps'
    /// ([`AstIdMap::entries`], [`RealSpanMap::anchors`]). An item no bit is set for has the
    /// ast ids and anchors it had, if [`Self::kept`] keeps it: its entries are all in that text,
    /// and an id is looked up by its node's kind and range, which no two entries share, so equal
    /// entries give equal lookups. Each map's arrays are walked once, old and new together.
    fn touched(
        &self,
        last: &Keyed,
        entries: &[[u32; 4]],
        pairs: &[[u32; 2]],
        starts: &[u32],
        items: &[ItemKey],
        touched: &mut [u64],
    ) {
        let mut mark = |offset: u32| {
            let at = items.partition_point(|it| u32::from(it.range.end()) <= offset);
            if items.get(at).is_some_and(|it| u32::from(it.range.start()) <= offset) {
                touched[at / 64] |= 1 << (at % 64);
            }
        };
        // The ast id map's first is the root's, which is no item's: its range is the whole
        // text. The span map's root anchor, before the first item's, is left out of a map whose
        // first item begins at 0, so it is told by its id.
        let old = (last.entries.get(1..).unwrap_or_default().iter())
            .filter_map(|&[s, e, kind, id]| self.moved(s, e).map(|[s, e]| [s, e, kind, id]));
        let new = entries.get(1..).unwrap_or_default().iter().copied();
        compare(old, new.filter(|&[s, e, ..]| self.keeps(s, e)), &mut mark);
        let root = ROOT_ERASED_FILE_AST_ID.into_raw();
        let old = (last.anchors.iter().filter(|it| it[1] != root))
            .filter_map(|&[s, id]| Some([self.moved(s, s + 1)?[0], id]));
        let new = (starts.iter().zip(pairs)).filter(|it| it.1[0] != root).map(|(&s, &[id, _])| [s, id]);
        compare(old, new.filter(|&[s, _]| self.keeps(s, s + 1)), &mut mark);
    }
}

/// Walks `old` and `new` together, each `[start, .., id]` of what is in the unchanged text
/// (`old`'s moved to where it is now), in their maps' order: a node's place and kind before
/// its id. Two of the same place and kind are a pair, which counts when their ids are equal
/// too; `mark` is called with the start of each that has no equal partner. The order is by
/// start within each run of rising starts (a level of the ast id map's breadth-first walk, or
/// the whole of the anchors), and a run is walked with the other side's matching run: when one
/// side's next starts a run and the other's does not, the other's is walked to its run's end.
/// A run out of step with the other side's only marks more; a pair never counts unless its
/// entries are equal.
fn compare<const N: usize>(
    old: impl Iterator<Item = [u32; N]>,
    new: impl Iterator<Item = [u32; N]>,
    mark: &mut impl FnMut(u32),
) {
    let (mut old, mut new) = (old.peekable(), new.peekable());
    let (mut old_at, mut new_at) = (0, 0);
    loop {
        let step = match (old.peek(), new.peek()) {
            (None, None) => return,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => match (a[0] < old_at, b[0] < new_at) {
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => a[..N - 1].cmp(&b[..N - 1]),
            },
        };
        match step {
            Ordering::Equal => {
                let (Some(a), Some(b)) = (old.next(), new.next()) else {
                    unreachable!("both sides peeked")
                };
                if a[N - 1] != b[N - 1] {
                    mark(b[0]);
                }
                (old_at, new_at) = (a[0], b[0]);
            }
            Ordering::Less => {
                let Some(a) = old.next() else { unreachable!("an old entry peeked") };
                mark(a[0]);
                old_at = a[0];
            }
            Ordering::Greater => {
                let Some(b) = new.next() else { unreachable!("a new entry peeked") };
                mark(b[0]);
                new_at = b[0];
            }
        }
    }
}

/// The lengths of the longest common prefix of `old` and `new` and of their longest common
/// suffix in what is left: one `memcmp` per 4 KiB at memory bandwidth, then per 64 bytes and
/// per byte to find where they differ.
fn common_ends(old: &[u8], new: &[u8]) -> (usize, usize) {
    let len = old.len().min(new.len());
    let mut prefix = 0;
    for step in [4096, 64, 1] {
        while prefix + step <= len && old[prefix..prefix + step] == new[prefix..prefix + step] {
            prefix += step;
        }
    }
    let (most, a, b) = (len - prefix, old.len(), new.len());
    let mut suffix = 0;
    for step in [4096, 64, 1] {
        while suffix + step <= most
            && old[a - suffix - step..a - suffix] == new[b - suffix - step..b - suffix]
        {
            suffix += step;
        }
    }
    (prefix, suffix)
}

macro_rules! marks {
    ($($column:ident),* $(,)?) => {
        /// Where a run of rows begins (or ends) in each column of a tree: the run of one or
        /// more top-level items. `kinds` is also the per-item `data` and `ast_ids`'.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub(super) struct Marks {
            $(pub(super) $column: u32,)*
        }

        impl Marks {
            /// Where the rows of `tree`'s items end: its columns' lengths, but for its top
            /// level's attribute owner, which is no item's.
            fn ends(tree: &ItemTree) -> Marks {
                let top =
                    tree.attr_owners().last().is_some_and(|it| it.owner == ModItemId::TOP_OWNER);
                let mut ends = Marks { $($column: base(tree.$column().len()),)* };
                ends.attr_owners -= top as u32;
                ends
            }

            /// This mark of a run that began at `from`, where the run begins at `to`.
            fn moved(&self, from: &Marks, to: &Marks) -> Marks {
                Marks { $($column: to.$column + (self.$column - from.$column),)* }
            }
        }

        /// The marks of the columns `sinks` are filling, named as the tree's.
        macro_rules! marks_of {
            ($sinks:ident) => { Marks { $($column: base($sinks.$column.len()),)* } };
        }
    };
}

marks!(
    top_level,
    kinds,
    children,
    consts,
    enums,
    extern_blocks,
    extern_crates,
    functions,
    impls,
    macro2s,
    macro_calls,
    macro_rules,
    mods,
    statics,
    structs,
    traits,
    type_aliases,
    unions,
    uses,
    use_trees,
    use_tree_lists,
    ctor_vis,
    names,
    paths,
    vis,
    attr_owners,
    attr_list,
    cfgs,
);

/// The rows of a run of top-level items of `tree`: `marks` are where each item's begin, then
/// where the last one's end.
pub(super) struct Rows<'a> {
    pub(super) tree: &'a ItemTree,
    pub(super) marks: &'a [Marks],
}

impl Rows<'_> {
    fn from(&self) -> &Marks {
        &self.marks[0]
    }

    fn to(&self) -> &Marks {
        self.marks.last().expect("a run of rows ends")
    }
}

/// A file's tree from its parts: `top`, its top level's attributes ([`top_attrs`]), and runs
/// of its items' rows in order, each run's ids moved by where its rows go. Every column's
/// length is summed first, then one pass over every part writes the tree's one block. Pushes
/// onto `marks` where each item's rows begin in the tree made, then where the last one's end.
/// The attributes are cloned, which allocates for those whose paths or inputs do.
pub(super) fn assemble(top: &ItemTree, parts: &[Rows<'_>], marks: &mut Vec<Marks>) -> ItemTree {
    macro_rules! len {
        ($column:ident) => {
            len!($column, $column)
        };
        ($column:ident, $by:ident) => {
            top.$column().len()
                + parts.iter().map(|it| (it.to().$by - it.from().$by) as usize).sum::<usize>()
        };
    }
    let items = len!(kinds);
    let lens = ItemTreeLens {
        top_level: len!(top_level),
        kinds: items,
        data: len!(data, kinds),
        ast_ids: len!(ast_ids, kinds),
        children: len!(children),
        consts: len!(consts),
        enums: len!(enums),
        extern_blocks: len!(extern_blocks),
        extern_crates: len!(extern_crates),
        functions: len!(functions),
        impls: len!(impls),
        macro2s: len!(macro2s),
        macro_calls: len!(macro_calls),
        macro_rules: len!(macro_rules),
        mods: len!(mods),
        statics: len!(statics),
        structs: len!(structs),
        traits: len!(traits),
        type_aliases: len!(type_aliases),
        unions: len!(unions),
        uses: len!(uses),
        use_trees: len!(use_trees),
        use_tree_lists: len!(use_tree_lists),
        ctor_vis: len!(ctor_vis),
        names: len!(names),
        paths: len!(paths),
        vis: len!(vis),
        attr_owners: len!(attr_owners),
        attr_index: items.div_ceil(64),
        attr_list: len!(attr_list),
        cfgs: len!(cfgs),
    };

    // alloc: the tree's block, once a tree at its length
    ItemTree::build(lens, |s| {
        s.attr_list.extend(top.attr_list().iter().cloned());
        s.cfgs.extend(top.cfgs().iter().cloned());
        for part in parts {
            assemble_part(s, part, marks);
        }
        marks.push(marks_of!(s));
        s.attr_owners.extend_from_slice(top.attr_owners());
        s.attr_index.extend(super::attr_blocks(s.attr_owners.written(), items));
    })
}

/// Writes `part`'s rows into the tree being assembled, after the rows written so far, each id
/// moved by where its run goes; pushes onto `marks` where each of its items' rows begin.
fn assemble_part(s: &mut ItemTreeSinks<'_>, part: &Rows<'_>, marks: &mut Vec<Marks>) {
    let (tree, from, to) = (part.tree, part.from(), part.to());
    macro_rules! rows {
        ($column:ident) => {
            rows!($column, $column)
        };
        ($column:ident, $by:ident) => {
            &tree.$column()[from.$by as usize..to.$by as usize]
        };
    }
    let now = marks_of!(s);
    marks.extend(part.marks[..part.marks.len() - 1].iter().map(|it| it.moved(from, &now)));
    // An id `i` of the part's is `i - from` in its run, so `i - from + now` in the tree.
    let by = |now: u32, from: u32| now.wrapping_sub(from);
    let base = Bases {
        item: by(now.kinds, from.kinds),
        children: by(now.children, from.children),
        use_trees: by(now.use_trees, from.use_trees),
        use_tree_lists: by(now.use_tree_lists, from.use_tree_lists),
        ctor_vis: by(now.ctor_vis, from.ctor_vis),
        names: by(now.names, from.names),
        paths: by(now.paths, from.paths),
        vis: by(now.vis, from.vis),
        attrs: by(now.attr_list, from.attr_list),
        cfgs: by(now.cfgs, from.cfgs),
    };
    for (&kind, &at) in rows!(kinds).iter().zip(rows!(data, kinds)) {
        let (column, origin) = match kind {
            ItemKind::Const => (now.consts, from.consts),
            ItemKind::Enum => (now.enums, from.enums),
            ItemKind::ExternBlock => (now.extern_blocks, from.extern_blocks),
            ItemKind::ExternCrate => (now.extern_crates, from.extern_crates),
            ItemKind::Function => (now.functions, from.functions),
            ItemKind::Impl => (now.impls, from.impls),
            ItemKind::Macro2 => (now.macro2s, from.macro2s),
            ItemKind::MacroCall => (now.macro_calls, from.macro_calls),
            ItemKind::MacroRules => (now.macro_rules, from.macro_rules),
            ItemKind::Mod => (now.mods, from.mods),
            ItemKind::Static => (now.statics, from.statics),
            ItemKind::Struct => (now.structs, from.structs),
            ItemKind::Trait => (now.traits, from.traits),
            ItemKind::TypeAlias => (now.type_aliases, from.type_aliases),
            ItemKind::Union => (now.unions, from.unions),
            ItemKind::Use => (now.uses, from.uses),
        };
        s.kinds.push(kind);
        s.data.push(by(column, origin).wrapping_add(at));
    }
    s.ast_ids.extend_from_slice(rows!(ast_ids, kinds));
    s.top_level.extend(rows!(top_level).iter().map(|&it| base.item(it)));
    s.children.extend(rows!(children).iter().map(|&it| base.item(it)));

    s.consts.extend(
        rows!(consts)
            .iter()
            .map(|it| Const { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.enums.extend(
        rows!(enums)
            .iter()
            .map(|it| Enum { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.extern_blocks.extend(
        rows!(extern_blocks).iter().map(|it| ExternBlock { children: base.children(it.children) }),
    );
    s.extern_crates.extend(rows!(extern_crates).iter().map(|it| ExternCrate {
        name: base.name(it.name),
        alias: base.alias(it.alias),
        visibility: base.vis(it.visibility),
    }));
    s.functions.extend(
        rows!(functions)
            .iter()
            .map(|it| Function { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.impls.extend_from_slice(rows!(impls));
    s.macro2s.extend(
        rows!(macro2s)
            .iter()
            .map(|it| Macro2 { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.macro_calls
        .extend(rows!(macro_calls).iter().map(|it| MacroCall { path: base.path(it.path), ..*it }));
    s.macro_rules
        .extend(rows!(macro_rules).iter().map(|it| MacroRules { name: base.name(it.name) }));
    s.mods.extend(rows!(mods).iter().map(|it| Mod {
        name: base.name(it.name),
        visibility: base.vis(it.visibility),
        items: match it.items {
            IdRange::NONE => IdRange::NONE,
            items => base.children(items),
        },
    }));
    s.statics.extend(
        rows!(statics)
            .iter()
            .map(|it| Static { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.structs.extend(rows!(structs).iter().map(|it| Struct {
        name: base.name(it.name),
        visibility: base.vis(it.visibility),
        value_ns_ctor: match it.value_ns_ctor.len {
            0 => it.value_ns_ctor,
            _ => shift(it.value_ns_ctor, base.ctor_vis),
        },
    }));
    s.traits.extend(
        rows!(traits)
            .iter()
            .map(|it| Trait { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.type_aliases.extend(
        rows!(type_aliases)
            .iter()
            .map(|it| TypeAlias { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.unions.extend(
        rows!(unions)
            .iter()
            .map(|it| Union { name: base.name(it.name), visibility: base.vis(it.visibility) }),
    );
    s.uses.extend(rows!(uses).iter().map(|it| Use {
        visibility: base.vis(it.visibility),
        use_tree: base.use_tree(it.use_tree),
    }));
    s.use_trees.extend(rows!(use_trees).iter().map(|it| UseTree {
        kind: it.kind,
        path: base.path(it.path),
        alias: base.alias(it.alias),
        list: match it.kind {
            UseTree::PREFIXED => shift(it.list, base.use_tree_lists),
            _ => it.list,
        },
    }));
    s.use_tree_lists.extend(rows!(use_tree_lists).iter().map(|&it| base.use_tree(it)));
    s.ctor_vis.extend(rows!(ctor_vis).iter().map(|&it| base.vis(it)));
    s.names.extend(rows!(names).iter().cloned());
    s.paths.extend(rows!(paths).iter().cloned());
    s.vis.extend(rows!(vis).iter().cloned());
    s.attr_owners.extend(rows!(attr_owners).iter().map(|it| AttrOwner {
        owner: base.item(it.owner),
        attrs: shift(it.attrs, base.attrs),
        cfg: match it.cfg {
            AttrOwner::ENABLED => AttrOwner::ENABLED,
            cfg => cfg.wrapping_add(base.cfgs),
        },
    }));
    s.attr_list.extend(rows!(attr_list).iter().cloned());
    s.cfgs.extend(rows!(cfgs).iter().cloned());
}

/// How one part's ids move into the tree assembled: added, wrapping, to each id of the part
/// that is not a sentinel ([`assemble`]).
struct Bases {
    item: u32,
    children: u32,
    use_trees: u32,
    use_tree_lists: u32,
    ctor_vis: u32,
    names: u32,
    paths: u32,
    vis: u32,
    attrs: u32,
    cfgs: u32,
}

fn base(len: usize) -> u32 {
    match u32::try_from(len) {
        Ok(len) if len < u32::MAX - 4 => len,
        _ => panic!("an item tree with {len} elements in one array: more than `u32` ids name"),
    }
}

fn shift(list: IdRange, by: u32) -> IdRange {
    IdRange { start: list.start.wrapping_add(by), len: list.len }
}

impl Bases {
    fn item(&self, it: ModItemId) -> ModItemId {
        ModItemId::new(it.index.wrapping_add(self.item))
    }

    fn children(&self, list: IdRange) -> IdRange {
        shift(list, self.children)
    }

    fn use_tree(&self, it: UseTreeId) -> UseTreeId {
        UseTreeId { raw: it.raw.wrapping_add(self.use_trees) }
    }

    fn name(&self, it: NameId) -> NameId {
        match it {
            NameId::NONE => it,
            NameId { raw } => NameId { raw: raw.wrapping_add(self.names) },
        }
    }

    fn alias(&self, it: AliasId) -> AliasId {
        match it {
            AliasId::NONE | AliasId::UNDERSCORE => it,
            AliasId { raw } => AliasId { raw: raw.wrapping_add(self.names) },
        }
    }

    fn path(&self, it: PathId) -> PathId {
        match it {
            PathId::NONE => it,
            PathId { raw } => PathId { raw: raw.wrapping_add(self.paths) },
        }
    }

    fn vis(&self, it: RawVisibilityId) -> RawVisibilityId {
        match it.is_builtin() {
            true => it,
            false => RawVisibilityId { raw: it.raw.wrapping_add(self.vis) },
        }
    }
}

/// The ids of `item` and the items it is made of, the nodes [`lower::Ctx`] asks the ids of, in
/// preorder (by start).
fn item_ids(item: ast::Item<'_>, map: &AstIdMap, start: TextSize, out: &mut Vec<[u32; 3]>) {
    push_id(out, item.syntax(), start, map.ast_id_of_view::<tag::Item>(item).erase());
    match item {
        ast::Item::Module(module) => {
            for child in module.item_list().into_iter().flat_map(|list| list.items()) {
                item_ids(child, map, start, out);
            }
        }
        ast::Item::ExternBlock(block) => {
            for child in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
                let child = ast::Item::from(child);
                let id = map.ast_id_of_view::<tag::Item>(child).erase();
                push_id(out, child.syntax(), start, id);
            }
        }
        _ => {}
    }
}

fn push_id(out: &mut Vec<[u32; 3]>, node: ast::Node<'_>, start: TextSize, id: ErasedFileAstId) {
    let at = u32::from(node.text_range().start() - start);
    out.push([at, node.kind() as u32, id.into_raw()]);
}

/// The anchors of `map` that start within `item` at one of its `ids`, from its start: the
/// item's own first, as every top-level item is one.
fn item_anchors(map: &RealSpanMap, item: TextRange, ids: &[[u32; 3]], out: &mut Vec<[u32; 2]>) {
    let from = out.len();
    let mut offset = item.end().checked_sub(TextSize::new(1)).unwrap_or(item.start());
    loop {
        let (start, id) = map.anchor_at(offset);
        assert!(
            start >= item.start(),
            "the top-level item at {item:?} is no anchor of its file's span map"
        );
        let at = u32::from(start - item.start());
        if ids.binary_search_by_key(&at, |it| it[0]).is_ok() {
            out.push([at, id.into_raw()]);
        }
        if start == item.start() {
            break;
        }
        offset = start - TextSize::new(1);
    }
    out[from..].reverse();
}

/// What collecting `item` reads of it, for an item name resolution defers behind an attribute:
/// its round-0 log carries this, so an edit of the item that keeps its ast id (a later
/// attribute, its visibility) still changes the log. [`hash_item`] with only the inner
/// attributes of its bodies and item lists ([`hash_inner_attr_nodes`]), so an edit within a
/// body that adds or removes no inner attribute leaves it equal. A query of its own so that it
/// alone reads the parse.
pub(crate) fn deferred_item_shape(db: &Db, item: crate::AstId<tag::Item>) -> u128 {
    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    fn deferred_item_shape(db: &Db, item: crate::AstId<tag::Item>, _: ()) -> u128 {
        let node = item.to_view(db);
        let mut bytes = Vec::new(); // alloc: one item's syntax, once per execution
        let mut feed = Feed(&mut bytes);
        hash_item(node.syntax(), &mut feed, hash_inner_attr_nodes);
        feed.fingerprint()
    }
    deferred_item_shape(db, item, ())
}

/// Hashes what lowering reads of `item`'s syntax ([`Ctx`](lower::Ctx)): all of it, but of the
/// bodies of its `fn`s and the items of its `impl`s and `trait`s what `hash_inner` hashes, and
/// of the bodies of its `const`s and `static`s nothing. With [`hash_inner_attrs`], nothing
/// lowering reads comes after what is left out within the node holding it, so leaving it out
/// moves nothing that is hashed; what it moves in the item ([`item_ids`], [`item_anchors`]) is
/// hashed with the key.
fn hash_item(
    item: ast::Node<'_>,
    state: &mut Feed<'_>,
    hash_inner: fn(ast::Node<'_>, &mut Feed<'_>),
) {
    let Some(ast) = ast::Item::cast(item) else { return hash_full(item, state) };
    let (inner, skipped, items) = match ast {
        ast::Item::Fn(it) => (element(it.body()), None, None),
        ast::Item::Impl(it) => (element(it.assoc_item_list()), None, None),
        ast::Item::Trait(it) => (element(it.assoc_item_list()), None, None),
        ast::Item::Const(it) => (None, element(it.body()), None),
        ast::Item::Static(it) => (None, element(it.body()), None),
        ast::Item::Module(it) => (None, None, element(it.item_list())),
        _ => (None, None, None),
    };
    if inner.is_none() && skipped.is_none() && items.is_none() {
        return hash_full(item, state);
    }
    (NODE, item.kind() as u16).hash(state);
    for child in item.children_with_tokens() {
        match child {
            NodeOrToken::Token(token) => hash_token(token, state),
            NodeOrToken::Node(node) if Some(node) == inner => hash_inner(node, state),
            NodeOrToken::Node(node) if Some(node) == skipped => SKIPPED.hash(state),
            NodeOrToken::Node(node) if Some(node) == items => {
                (NODE, node.kind() as u16).hash(state);
                for child in node.children_with_tokens() {
                    match child {
                        NodeOrToken::Token(token) => hash_token(token, state),
                        NodeOrToken::Node(node) => hash_item(node, state, hash_inner),
                    }
                    END.hash(state);
                }
            }
            NodeOrToken::Node(node) => hash_full(node, state),
        }
        END.hash(state);
    }
}

/// Hashes the inner attributes of a `fn`'s body or an `impl`'s or `trait`'s list of items, what
/// attribute lowering reads of it ([`ast::HasAttrs::inner_attributes_node`]): the tokens,
/// attributes and doc comments before its first statement or item, and after that any inner
/// attribute or doc comment with its offset. A lazy body is hashed whole.
fn hash_inner_attrs(node: ast::Node<'_>, state: &mut Feed<'_>) {
    if node.is_lazy() {
        return hash_full(node, state);
    }
    (NODE, node.kind() as u16).hash(state);
    let start = node.text_range().start();
    let mut leading = true;
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::STMT_LIST => {
                hash_inner_attrs(node, state)
            }
            NodeOrToken::Node(node) if ast::AnyAttr::can_cast(node.kind()) => {
                let attr = ast::AnyAttr::cast(node).expect("an attribute");
                if !leading {
                    if !attr.kind().is_inner() {
                        continue;
                    }
                    u32::from(node.text_range().start() - start).hash(state);
                }
                hash_full(node, state);
            }
            NodeOrToken::Node(_) => leading = false,
            NodeOrToken::Token(token) if leading => hash_token(token, state),
            NodeOrToken::Token(_) => {}
        }
    }
    END.hash(state);
}

/// Hashes the inner attributes of a `fn`'s body or an `impl`'s or `trait`'s list of items, in
/// order and without their offsets or the tokens around them: what collecting the item reads of
/// it. A lazy body that surely starts with none hashes as one with none, and one that may is
/// hashed whole.
fn hash_inner_attr_nodes(node: ast::Node<'_>, state: &mut Feed<'_>) {
    if node.is_lazy() {
        if ast::block_may_start_with_inner_attr(node.text(), false) {
            hash_full(node, state);
        }
        return END.hash(state);
    }
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::STMT_LIST => {
                hash_inner_attr_nodes(node, state)
            }
            NodeOrToken::Node(node) => {
                if ast::AnyAttr::cast(node).is_some_and(|attr| attr.kind().is_inner()) {
                    hash_full(node, state);
                }
            }
            NodeOrToken::Token(_) => {}
        }
    }
    END.hash(state);
}

/// The node a view is, to find among its parent's children.
fn element<'t>(node: Option<impl AstNode<'t>>) -> Option<ast::Node<'t>> {
    Some(node?.syntax())
}

const NODE: u8 = 0;
const TOKEN: u8 = 1;
const SKIPPED: u8 = 2;
const END: u8 = 3;
const KEPT_HASH: u8 = 4;

/// Subtrees of this much text keep their hash in their tree ([`syntax::FlatTree::subtree_hash`]),
/// across edits that don't touch them.
const KEPT_TEXT: u32 = 4096;

fn hash_full(node: ast::Node<'_>, state: &mut Feed<'_>) {
    let (tree, at) = node.flat();
    if u32::from(node.text_range().len()) >= KEPT_TEXT {
        return (KEPT_HASH, tree.subtree_hash(at)).hash(state);
    }
    NODE.hash(state);
    tree.hash_subtree(at, state);
}

fn hash_token(token: ast::Token<'_>, state: &mut Feed<'_>) {
    (TOKEN, token.kind() as u16, token.text()).hash(state);
}
