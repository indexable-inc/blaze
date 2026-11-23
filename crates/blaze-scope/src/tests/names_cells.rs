//! Names read by cell (names_cells.rs): two crates (k1 uses k0) solved before and after a
//! private fn is added to k0's module `a`, and every path of a family walked from every module
//! of both crates through [`Logged`].
//!
//! The oracle, per walk (rule 21, the cells cover the walk): when the answer before and after
//! differ, a cell the walk logged before holds another digest after; and each logged cell's
//! digest is the same from the table and from the record (the two homes agree). The floor
//! (rule 67): the edit moves ONE cell of k0's table, the new name's binding, and none of k1's;
//! a walk that did not name `added` consulted no moved cell. The control: the same oracle over
//! logs with the binding cells dropped (a `get` not logged) is caught.

use std::format;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_surface::{ItemKind, Namespace, PathKind};

use super::entry::{Texts, input};
use crate::cell_key::cell_key;
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names::{Written, path};
use crate::items_record::{ItemsRecord, TextOf, encode as items_encode};
use crate::names_cells::{Log, Logged, NameCell, Section, beginning_digest, first_16, seen_by_record, seen_in, table};
use crate::names_record::{LoadNames, NamesRecord, RecordNames, Verified, encode};
use crate::resolve::aligned;
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const K0: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub mod b {
        pub fn f() {}
        pub use super::S as T;
    }
}
pub mod c {
    pub use crate::a::b::*;
    pub use crate::a::*;
    pub enum E { V, W {} }
}
pub use c::E;
";

const K0_ADDED: &str = "#![no_core]
pub mod a {
    pub struct S;
    fn added() {}
    pub mod b {
        pub fn f() {}
        pub use super::S as T;
    }
}
pub mod c {
    pub use crate::a::b::*;
    pub use crate::a::*;
    pub enum E { V, W {} }
}
pub use c::E;
";

const K1: &str = "#![no_core]
pub mod m {
    pub use k0::c::E;
    pub use k0::a::*;
    pub fn g() {}
}
pub use m::g;
";

pub(super) static BEFORE: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1)]]);
static AFTER: Texts = Texts(&[&[("/k0/lib.rs", K0_ADDED)], &[("/k1/lib.rs", K1)]]);

struct Loaded(Vec<&'static [u8]>);

impl<'a> LoadNames<'a> for Loaded {
    fn load(&self, krate: usize) -> Option<&'a [u8]> {
        self.0.get(krate).copied()
    }
}

/// One side of the edit: each crate's names record, opened, and its cell table; and the
/// table with the items record's cells and a `Whole(Items)` over its bytes.
pub(super) struct Side {
    pub(super) bytes: Vec<&'static [u8]>,
    pub(super) keys: Vec<CrateItemKey>,
    tables: Vec<Vec<u8>>,
    with_items: Vec<Vec<u8>>,
    items: Vec<&'static [u8]>,
}

impl Side {
    pub(super) fn solve(texts: &'static Texts) -> Side {
        let bump = super::SyncFrame::leaked();
        let g = bump.alloc();
        let inputs = [input("k0", "/k0/lib.rs", &[]), input("k1", "/k1/lib.rs", &[("k0", 0)])];
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
        let mut side = Side { bytes: Vec::new(), keys: Vec::new(), tables: Vec::new(), with_items: Vec::new(), items: Vec::new() };
        for (i, s) in solved.iter().enumerate() {
            assert!(s.names.notes.is_empty(), "crate {i}: notes {:?}", s.names.notes);
            let bytes = aligned(&encode(s.names, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump);
            let record = side.record_of(bytes);
            side.tables.push(table(&record, None, &[], g).unwrap_or_else(|e| panic!("crate {i}'s cell table: {e:?}")).to_vec());
            let items_bytes = aligned(&items_encode(s.names, g).unwrap_or_else(|e| panic!("items encode: {e:?}")), bump);
            let text: &'static str = texts.0[i][0].1;
            let items = super::items_record::one_file(items_bytes, text, bump);
            side.items.push(items_bytes);
            let wholes = [(Section::Items, items_bytes)];
            let read = move |of: TextOf<'_>| -> Option<&'static str> { matches!(of, TextOf::File { index: 0, .. }).then_some(text) };
            side.with_items.push(table(&record, Some((&items, &read)), &wholes, g).unwrap_or_else(|e| panic!("crate {i}'s items cells: {e:?}")).to_vec());
            side.keys.push(s.names.key);
            side.bytes.push(bytes);
        }
        side
    }

    fn record_of(&self, bytes: &'static [u8]) -> NamesRecord<'static> {
        NamesRecord::open(Verified::check(bytes).unwrap_or_else(|e| panic!("a record does not check: {e:?}")))
    }

    pub(super) fn record(&self, krate: usize) -> NamesRecord<'static> {
        self.record_of(self.bytes[krate])
    }

    /// The crate a cell is of, by its keys' prefix.
    fn crate_of(&self, cell: NameCell) -> Option<usize> {
        self.keys.iter().position(|k| k.prefix() == cell.crate_prefix())
    }

    /// What the cell holds on this side, from the table.
    fn seen(&self, key: [u8; 16], cell: NameCell) -> Option<ContentId> {
        let krate = self.crate_of(cell)?;
        seen_in(&self.tables[krate], key).unwrap_or_else(|e| panic!("crate {krate}'s table: {e:?}"))
    }
}

/// The keys of the rows two tables do not share: a cell added, gone or holding another digest.
fn moved(before: &[u8], after: &[u8]) -> Vec<[u8; 16]> {
    let rows = |t: &[u8]| -> Vec<([u8; 16], [u8; 32])> {
        t.chunks(48)
            .map(|r| {
                let (mut k, mut d) = ([0; 16], [0; 32]);
                k.copy_from_slice(&r[..16]);
                d.copy_from_slice(&r[16..]);
                (k, d)
            })
            .collect()
    };
    let (b, a) = (rows(before), rows(after));
    let mut out: Vec<[u8; 16]> = b.iter().filter(|r| !a.contains(r)).chain(a.iter().filter(|r| !b.contains(r))).map(|r| r.0).collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub(super) fn family() -> Vec<(PathKind, Vec<&'static str>)> {
    let leaves = ["a", "b", "c", "m", "S", "T", "f", "g", "E", "V", "W", "k0", "added", "missing"];
    let mut all = Vec::new();
    for n in leaves {
        all.push((PathKind::Plain, std::vec![n]));
        all.push((PathKind::Crate, std::vec![n]));
        all.push((PathKind::Abs, std::vec!["k0", n]));
        for a in ["a", "c", "m", "E", "super", "self"] {
            all.push((PathKind::Plain, std::vec![a, n]));
            all.push((PathKind::Crate, std::vec![a, n]));
            all.push((PathKind::Plain, std::vec!["k0", a, n]));
        }
    }
    all
}

/// Every walk of the family from every module of both crates, before and after: the walks
/// whose answer moved with no logged cell moved, those whose cells disagree between the table
/// and the record, and how many walks consulted a moved cell. `drop_bindings`: the control,
/// the logs' binding cells left out.
fn judge(before: &Side, after: &Side, drop_bindings: bool) -> (Vec<std::string::String>, Vec<std::string::String>, usize, usize) {
    let g = blaze_own::testing::heap().alloc();
    let world_before = RecordNames::new(&before.keys, Loaded(before.bytes.clone()), g);
    let world_after = RecordNames::new(&after.keys, Loaded(after.bytes.clone()), g);
    let (mut unsound, mut disagree, mut dirtied, mut walks) = (Vec::new(), Vec::new(), 0, 0);
    for krate in 0..before.keys.len() {
        let modules: Vec<ItemKey> = before.record(krate).modules().collect();
        for module in modules {
            for (kind, segments) in family() {
                for ns in [Namespace::Types, Namespace::Values] {
                    walks += 1;
                    let written = Written { kind, segments: &segments };
                    let log = Log::new(g);
                    let logged = Logged { world: &world_before, log: &log };
                    let was = path(&logged, module, ns, written, g);
                    let now = path(&world_after, module, ns, written, g);
                    let cells = log.cells(g);
                    let mut any_moved = false;
                    for (key, cell) in cells.iter() {
                        if drop_bindings && matches!(cell, NameCell::Binding { .. }) {
                            continue;
                        }
                        let seen = before.seen(*key, *cell);
                        let by_record = before.crate_of(*cell).and_then(|k| seen_by_record(&before.record(k), None, &[], *cell).unwrap_or_else(|e| panic!("{cell:?}: {e:?}")));
                        if seen != by_record {
                            disagree.push(format!("{cell:?}: table {seen:?}, record {by_record:?}"));
                        }
                        any_moved |= seen != after.seen(*key, *cell);
                    }
                    if was != now && !any_moved {
                        unsound.push(format!("crate {krate} module {module:?} `{kind:?} {}` {ns:?}: {was:?} then {now:?}", segments.join("::")));
                    }
                    dirtied += usize::from(any_moved);
                }
            }
        }
    }
    (unsound, disagree, dirtied, walks)
}

#[test]
fn a_private_fn_added_moves_its_own_binding_cell_alone() {
    let (before, after) = (Side::solve(&BEFORE), Side::solve(&AFTER));
    assert_eq!(before.keys, after.keys, "the crates' keys moved with a private fn");
    let a = {
        let g = blaze_own::testing::heap().alloc();
        let root = before.record(0).root_module();
        let found = path(&before.record(0), root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["a"] }, g);
        found.ok().and_then(|r| r.whole(1)).unwrap_or_else(|| panic!("k0 has no module `a`: {found:?}")).key
    };
    let added = NameCell::Binding { module: a, name: cell_key(Namespace::Values, "added") }.key();
    let k0 = moved(&before.tables[0], &after.tables[0]);
    let k1 = moved(&before.tables[1], &after.tables[1]);
    std::println!("NAMES CELLS: a private fn added moved {} cell(s) of k0 (floor 1), {} of k1 (floor 0)", k0.len(), k1.len());
    let mut named: Vec<NameCell> = std::vec![NameCell::Root { krate: before.keys[0].prefix() }];
    named.extend(before.record(0).modules().map(|module| NameCell::Module { module }));
    named.extend(before.record(0).enums().map(|owner| NameCell::Variants { owner }));
    let which: Vec<std::string::String> = k0.iter().map(|k| named.iter().find(|c| c.key() == *k).map_or(format!("{k:?}"), |c| format!("{c:?}"))).collect();
    assert_eq!(k0, std::vec![added], "k0's moved cells are not the new binding's alone: {which:?}");
    assert!(k1.is_empty(), "k1's cells moved: {k1:?}");

    // The items cells: the new item's row and the coarse whole section; no other item's row
    // (an item's place is not its cell).
    let z = path(&after.record(0), a, Namespace::Values, Written { kind: PathKind::Plain, segments: &["added"] }, blaze_own::testing::heap().alloc());
    let z = z.ok().and_then(|r| r.whole(1)).unwrap_or_else(|| panic!("`added` does not resolve in `a`: {z:?}")).key;
    // `mod a { .. }` as an item of the items record: its row's key, not its scope's.
    let a_item = {
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let items = super::items_record::one_file(after.items[0], K0_ADDED, erased.bump());
        (0..items.items()).filter_map(|i| items.item_at(i).ok().flatten()).find(|row| row.kind == ItemKind::Module && row.name == Some("a")).map(|row| row.key)
    };
    let a_item = a_item.unwrap_or_else(|| panic!("k0's items record has no row of `mod a`"));
    // The row of `mod a` holds its head only (`module_head`): the fn added inside it moves the
    // module's own cells, never the parent's cell of that row.
    // And the new key's own `Beginning` (its 16-byte prefix now begins a key: lane ND).
    let mut floor = std::vec![added, NameCell::Item { key: z }.key(), NameCell::Beginning { first: first_16(z) }.key(), NameCell::Whole { krate: before.keys[0].prefix(), section: Section::Items }.key()];
    floor.sort_unstable();
    let items_moved = moved(&before.with_items[0], &after.with_items[0]);
    std::println!("NAMES CELLS: with the items record, {} cell(s) of k0 moved (floor 4: the binding, the item row, its prefix's beginning, the whole section)", items_moved.len());
    assert!(!items_moved.contains(&NameCell::Item { key: a_item }.key()), "the row of `mod a` moved with an item added inside it");
    let mut candidates: Vec<(std::string::String, [u8; 16])> = std::vec![("Item(added)".into(), NameCell::Item { key: z }.key()), ("Beginning(added)".into(), NameCell::Beginning { first: first_16(z) }.key())];
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let items_after = super::items_record::one_file(after.items[0], K0_ADDED, erased.bump());
    for i in 0..items_after.items() {
        if let Ok(Some(row)) = items_after.item_at(i) {
            candidates.push((format!("Item({:?} {:?})", row.kind, row.name), NameCell::Item { key: row.key }.key()));
            candidates.push((format!("Members({:?} {:?})", row.kind, row.name), NameCell::Members { owner: row.key }.key()));
        }
    }
    for (i, m) in before.record(0).modules().enumerate() {
        candidates.push((format!("Item(module {i}{})", if m == a { " = a" } else { "" }), NameCell::Item { key: m }.key()));
        candidates.push((format!("Members(module {i})"), NameCell::Members { owner: m }.key()));
    }
    let which: Vec<std::string::String> = items_moved.iter().map(|k| candidates.iter().find(|c| c.1 == *k).map_or(format!("{k:?}"), |c| c.0.clone())).collect();
    assert_eq!(items_moved, floor, "k0's moved cells with its items are not the floor: {which:?}");
    assert_eq!(before.with_items[1], after.with_items[1], "k1's items cells moved");

    let (unsound, disagree, dirtied, walks) = judge(&before, &after, false);
    std::println!("NAMES CELLS: {dirtied} of {walks} walks consulted a moved cell");
    assert!(unsound.is_empty(), "walks whose answer moved with no logged cell moved:\n{}", unsound.join("\n"));
    assert!(disagree.is_empty(), "cells whose table and record digests differ:\n{}", disagree.join("\n"));
    assert!(dirtied > 0, "no walk consulted the new binding: the family does not ask `added`");
}

/// A module's head ends at its body or its `;`, past attributes and comments whose brackets,
/// strings and braces would end it early.
#[test]
fn a_module_head_stops_at_its_body() {
    use crate::names_cells::module_head;
    let cases: [(&str, &str); 7] = [
        ("pub mod a { fn f() {} }", "pub mod a "),
        ("mod a;", "mod a"),
        ("#[cfg(x)] pub(crate) mod a {}", "#[cfg(x)] pub(crate) mod a "),
        ("#[doc = \"a { ; b\"] mod a {}", "#[doc = \"a { ; b\"] mod a "),
        ("#[doc = r#\"x\"{\"#] mod a {}", "#[doc = r#\"x\"{\"#] mod a "),
        ("/// a { b\n/* { /* ; */ } */ mod a {}", "/// a { b\n/* { /* ; */ } */ mod a "),
        ("#[x(y = '{')] mod a {}", "#[x(y = '{')] mod a "),
    ];
    for (text, head) in cases {
        assert_eq!(module_head(text), head, "the head of {text:?}");
    }
}

#[test]
fn control_a_get_not_logged_is_caught() {
    let (before, after) = (Side::solve(&BEFORE), Side::solve(&AFTER));
    let (unsound, _, _, _) = judge(&before, &after, true);
    assert!(!unsound.is_empty(), "the oracle did not see the binding cells left out of the logs");
}

/// THE HOLE `key_beginning` HAD (rule 21), at the cell level: two real keys (BLAKE3) cannot be
/// made to share 16 bytes in a fixture, so the keys here are made by hand. A body asking
/// `key_beginning(p)` is answered "the one key beginning so" only while no second key begins
/// so; the found path read only `Item(a)`, a row keyed by `a` alone, which a second key `b`
/// added with the prefix does not touch (it adds a row of its own key). `Beginning(p)` holds
/// every key beginning so, in each list, so adding `b` to any list moves it.
#[test]
fn a_second_key_with_a_prefix_moves_its_beginning() {
    let key = |tail: u8| {
        let mut bytes = [7u8; 32];
        bytes[20] = tail;
        ItemKey(ContentId(bytes))
    };
    let (a, b) = (key(1), key(2));
    let p = first_16(a);
    assert_eq!(p, first_16(b), "the two hand-made keys share their first 16 bytes");
    // The old read: `Item(a)` names `a` alone, and `b` is another cell.
    assert_ne!(NameCell::Item { key: a }.key(), NameCell::Item { key: b }.key());
    let one = beginning_digest(p, [&[a], &[], &[]]);
    assert!(one.is_some(), "a prefix one key begins is a row");
    for (list, lists) in [("items", [&[a, b][..], &[][..], &[][..]]), ("modules", [&[a][..], &[b][..], &[][..]]), ("variants", [&[a][..], &[][..], &[b][..]])] {
        assert_ne!(one, beginning_digest(p, lists), "a second key with the prefix in the {list} did not move `Beginning`");
    }
    // A key in another list is another answer (an item row is searched before a module row).
    assert_ne!(beginning_digest(p, [&[a], &[], &[]]), beginning_digest(p, [&[], &[a], &[]]));
    // No key beginning so: absent, and a key with another prefix leaves the cell alone.
    let mut other = [7u8; 32];
    other[3] = 9;
    let other = ItemKey(ContentId(other));
    assert_eq!(beginning_digest(p, [&[], &[], &[]]), None);
    assert_eq!(beginning_digest(p, [&[], &[other], &[]]), None);
    let mut sorted = [a, other];
    sorted.sort_unstable();
    assert_eq!(one, beginning_digest(p, [&sorted, &[], &[]]), "a key of another prefix moved `Beginning`");
}

/// k0 with a trait and an impl, so the member owners section has rows.
const K0_MEMBERS: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub mod b {
        pub fn f() {}
        pub use super::S as T;
    }
}
pub mod c {
    pub use crate::a::b::*;
    pub use crate::a::*;
    pub enum E { V, W {} }
}
pub use c::E;
pub trait Tr {
    fn m(&self);
    const C: u8;
}
impl Tr for a::S {
    fn m(&self) {}
    const C: u8 = 0;
}
";

static MEMBERS: Texts = Texts(&[&[("/k0/lib.rs", K0_MEMBERS)], &[("/k1/lib.rs", K1)]]);

/// Every `MemberOf`, `VariantOf` and `Beginning` a key of the crates' records can name, found
/// and absent, holds the same digest in the table as read from the record by its own search
/// (`seen_by_record`, the cell's second home).
#[test]
fn the_items_search_cells_agree_with_the_record() {
    let side = Side::solve(&MEMBERS);
    let bump = super::SyncFrame::leaked();
    let (mut members, mut variants, mut beginnings, mut absent) = (0usize, 0usize, 0usize, 0usize);
    for krate in 0..side.keys.len() {
        let text: &'static str = MEMBERS.0[krate][0].1;
        let items = super::items_record::one_file(side.items[krate], text, bump);
        let read = move |of: TextOf<'_>| -> Option<&'static str> { matches!(of, TextOf::File { index: 0, .. }).then_some(text) };
        let record = side.record(krate);
        let wholes = [(Section::Items, side.items[krate])];
        let mut cells: Vec<NameCell> = Vec::new();
        let mut fake = [0xeeu8; 32];
        fake[..8].copy_from_slice(&side.keys[krate].prefix());
        let fake = ItemKey(ContentId(fake));
        cells.extend([NameCell::MemberOf { member: fake }, NameCell::VariantOf { variant: fake }, NameCell::Beginning { first: first_16(fake) }]);
        for i in 0..items.items() {
            if let Ok(Some(row)) = items.item_at(i) {
                cells.extend([NameCell::MemberOf { member: row.key }, NameCell::VariantOf { variant: row.key }, NameCell::Beginning { first: first_16(row.key) }]);
            }
        }
        for i in 0..items.modules() {
            if let Ok(Some(row)) = items.module_at(i) {
                cells.push(NameCell::Beginning { first: first_16(row.key) });
            }
        }
        for i in 0..items.variant_rows() {
            if let Ok(Some((variant, ..))) = items.variant_at(i) {
                cells.extend([NameCell::VariantOf { variant }, NameCell::Beginning { first: first_16(variant) }, NameCell::MemberOf { member: variant }]);
            }
        }
        for i in 0..items.owned_members() {
            if let Ok(Some(member)) = items.owned_key_at(i) {
                cells.extend([NameCell::MemberOf { member }, NameCell::VariantOf { variant: member }]);
            }
        }
        for cell in cells {
            let in_table = seen_in(&side.with_items[krate], cell.key()).unwrap_or_else(|e| panic!("crate {krate}'s table: {e:?}"));
            let by_record = seen_by_record(&record, Some((&items, &read)), &wholes, cell).unwrap_or_else(|e| panic!("{cell:?}: {e:?}"));
            assert_eq!(in_table, by_record, "crate {krate} {cell:?}: the table and the record disagree");
            match (cell, in_table) {
                (_, None) => absent += 1,
                (NameCell::MemberOf { .. }, Some(_)) => members += 1,
                (NameCell::VariantOf { .. }, Some(_)) => variants += 1,
                (NameCell::Beginning { .. }, Some(_)) => beginnings += 1,
                _ => {}
            }
        }
    }
    std::println!("NAMES CELLS: search cells found: {members} member, {variants} variant, {beginnings} beginning; {absent} absent");
    assert!(members >= 4, "the impl's and the trait's members are not all `MemberOf` rows: {members}");
    assert!(variants >= 2, "`E`'s variants are not `VariantOf` rows: {variants}");
    assert!(beginnings > 0 && absent > 0, "no beginning found ({beginnings}) or none absent ({absent})");
}

/// Foreign items written every way std and libc write them: by a macro call's expansion
/// (`decl!`), through a nested macro inside a `mod` (`outer!` calls `decl_static!`), by hand
/// with `extern "system"`, and by hand with a bare `extern`.
const FOREIGN: &str = r#"#![no_core]
macro_rules! decl { () => { extern "C" { fn abs(x: i32) -> i32; } } }
decl!();
macro_rules! decl_static { () => { extern { static environ: u8; } } }
macro_rules! outer { () => { decl_static!(); } }
pub mod m {
    outer!();
}
extern "system" { fn sys(); }
extern { fn bare(); }
pub fn own() {}
"#;

/// [`FOREIGN`] with the hand-written `extern "system"` block's string edited, nothing else.
const FOREIGN_EDITED: &str = r#"#![no_core]
macro_rules! decl { () => { extern "C" { fn abs(x: i32) -> i32; } } }
decl!();
macro_rules! decl_static { () => { extern { static environ: u8; } } }
macro_rules! outer { () => { decl_static!(); } }
pub mod m {
    outer!();
}
extern "C-unwind" { fn sys(); }
extern { fn bare(); }
pub fn own() {}
"#;

static FOREIGN_BEFORE: Texts = Texts(&[&[("/k0/lib.rs", FOREIGN)]]);
static FOREIGN_AFTER: Texts = Texts(&[&[("/k0/lib.rs", FOREIGN_EDITED)]]);

/// One crate solved: its names record's bytes and its items record's bytes.
fn foreign_side(texts: &'static Texts) -> (&'static [u8], &'static [u8]) {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", "/k0/lib.rs", &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let s = &solved[0];
    assert!(s.names.notes.is_empty(), "notes {:?}", s.names.notes);
    let names = aligned(&encode(s.names, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump);
    let items = aligned(&items_encode(s.names, g).unwrap_or_else(|e| panic!("items encode: {e:?}")), bump);
    (names, items)
}

/// Each named item row of the record: (name, key, its text's place is an expansion, `foreign`).
fn foreign_rows(items: &ItemsRecord<'static>) -> Vec<(&'static str, ItemKey, bool, Option<&'static str>)> {
    let mut rows = Vec::new();
    for i in 0..items.items() {
        let row = items.item_at(i).unwrap_or_else(|e| panic!("item row {i}: {e:?}")).unwrap_or_else(|| panic!("item row {i} is absent"));
        if let Some(name) = row.name {
            let in_expansion = matches!(row.text.map(|t| t.at), Some(crate::resolve::At::Expansion(_)));
            rows.push((name, row.key, in_expansion, row.foreign));
        }
    }
    rows
}

/// THE RULE (F1, 2026-11-17): what a foreign item's ABI is, is a fact of the item, written once
/// by the names task into its items row, whoever wrote the item. Over records `extern_abi` was
/// refused for a bodiless fn or static an expansion wrote ("the expansion's outline is not made
/// again over records"), which stopped std's hello world.
///
/// Shows: (a) `abs`, written by `decl!`'s expansion, holds "C" and its text is the expansion's;
/// (b) `environ`, written two macros deep inside `mod m`, holds "C" (a bare `extern`) and is
/// an expansion's too, a static; (c) `sys` holds "system" and `bare` "C"; `own` (a fn with a
/// body, in no block) holds none. (e) the control: editing `sys`'s block string moves `sys`'s
/// `Item` cell and leaves `own`'s alone, and the row then holds "C-unwind".
///
/// A writer that read the ABI only from a file's outline fails (a) and (b); one that wrote
/// "C" for every block fails (c); an `Item` digest that left the field out fails (e).
#[test]
fn a_foreign_items_block_abi_is_in_its_row() {
    let g = super::SyncFrame::leaked();
    let (names_before, items_before) = foreign_side(&FOREIGN_BEFORE);
    let (names_after, items_after) = foreign_side(&FOREIGN_AFTER);
    let before = super::items_record::one_file(items_before, FOREIGN, g);
    let after = super::items_record::one_file(items_after, FOREIGN_EDITED, g);
    let rows = foreign_rows(&before);
    let find = |rows: &[(&'static str, ItemKey, bool, Option<&'static str>)], name: &str| -> (ItemKey, bool, Option<&'static str>) {
        rows.iter().find(|r| r.0 == name).map(|r| (r.1, r.2, r.3)).unwrap_or_else(|| panic!("no item row named `{name}` among {rows:?}"))
    };
    let (_, abs_in_expansion, abs) = find(&rows, "abs");
    assert!(abs_in_expansion, "`abs` is not placed in `decl!`'s expansion");
    assert_eq!(abs, Some("C"), "`abs` (an expansion's `extern \"C\"`)");
    let (_, environ_in_expansion, environ) = find(&rows, "environ");
    assert!(environ_in_expansion, "`environ` is not placed in `decl_static!`'s expansion");
    assert_eq!(environ, Some("C"), "`environ` (a bare `extern` two macros deep in `mod m`)");
    let (sys_key, _, sys) = find(&rows, "sys");
    assert_eq!(sys, Some("system"), "`sys` (a hand-written `extern \"system\"`)");
    assert_eq!(find(&rows, "bare").2, Some("C"), "`bare` (a hand-written bare `extern`)");
    let (own_key, _, own) = find(&rows, "own");
    assert_eq!(own, None, "`own` is in no `extern` block");

    // (e) The edit of the block's string, at the cell level.
    let rows_after = foreign_rows(&after);
    let (sys_key_after, _, sys_after) = find(&rows_after, "sys");
    assert_eq!(sys_after, Some("C-unwind"), "`sys` after its block's string was edited");
    let (own_key_after, ..) = find(&rows_after, "own");
    let digest = |names: &'static [u8], items_bytes: &'static [u8], text: &'static str, key: ItemKey| -> Option<ContentId> {
        let items = super::items_record::one_file(items_bytes, text, g);
        let record = NamesRecord::open(Verified::check(names).unwrap_or_else(|e| panic!("a record does not check: {e:?}")));
        let read = move |of: TextOf<'_>| -> Option<&'static str> { matches!(of, TextOf::File { index: 0, .. }).then_some(text) };
        let wholes = [(Section::Items, items_bytes)];
        seen_by_record(&record, Some((&items, &read)), &wholes, NameCell::Item { key }).unwrap_or_else(|e| panic!("the `Item` cell of {key:?}: {e:?}"))
    };
    let sys_was = digest(names_before, items_before, FOREIGN, sys_key);
    let sys_now = digest(names_after, items_after, FOREIGN_EDITED, sys_key_after);
    assert!(sys_was.is_some() && sys_now.is_some(), "`sys` has no `Item` cell");
    assert_ne!(sys_was, sys_now, "editing `sys`'s block ABI did not move its `Item` cell");
    assert_eq!(digest(names_before, items_before, FOREIGN, own_key), digest(names_after, items_after, FOREIGN_EDITED, own_key_after), "an edit of another item's block moved `own`'s `Item` cell");
    std::println!("FOREIGN ROWS: abs {abs:?} (expansion), environ {environ:?} (nested expansion), sys {sys:?} then {sys_after:?}, own {own:?}; sys key same across the edit: {}", sys_key == sys_key_after);
}

#[test]
fn impl_rows_and_files_cells_stand_on_private_fn_and_move_on_own_change() {
    const BASE: &str = "#![no_core]\npub struct S;\nimpl S { pub fn a() {} }\n";
    const WITH_PRIV_FN: &str = "#![no_core]\npub struct S;\nimpl S { pub fn a() {} }\nfn __priv() {}\n";
    const WITH_IMPL: &str = "#![no_core]\npub struct S;\nimpl S { pub fn a() {} }\nimpl S { pub fn b() {} }\n";
    const WITH_FILE: &str = "#![no_core]\npub struct S;\nimpl S { pub fn a() {} }\nmod extra;\n";
    const DEP: &str = "#![no_core]\npub fn dep() {}\n";

    static T_BASE: Texts = Texts(&[&[("/k0/lib.rs", BASE)], &[("/k1/lib.rs", DEP)]]);
    static T_PRIV: Texts = Texts(&[&[("/k0/lib.rs", WITH_PRIV_FN)], &[("/k1/lib.rs", DEP)]]);
    static T_IMPL: Texts = Texts(&[&[("/k0/lib.rs", WITH_IMPL)], &[("/k1/lib.rs", DEP)]]);
    static T_FILE: Texts = Texts(&[&[("/k0/lib.rs", WITH_FILE), ("/k0/extra.rs", "")], &[("/k1/lib.rs", DEP)]]);

    let base = Side::solve(&T_BASE);
    let priv_fn = Side::solve(&T_PRIV);
    let with_impl = Side::solve(&T_IMPL);
    let with_file = Side::solve(&T_FILE);

    let krate = base.keys[0].prefix();
    let impl_cell = NameCell::ImplRows { krate };
    let files_cell = NameCell::Files { krate };

    let lookup = |side: &Side, cell: NameCell| -> Option<ContentId> {
        let table_val = seen_in(&side.with_items[0], cell.key()).expect("seen_in");
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let items = super::items_record::one_file(side.items[0], "", erased.bump());
        let record = side.record(0);
        let read = |_: TextOf<'_>| -> Option<&str> { None };
        let wholes = [(Section::Items, side.items[0])];
        let rec_val = seen_by_record(&record, Some((&items, &read)), &wholes, cell).expect("seen_by_record");
        assert_eq!(table_val, rec_val, "table and seen_by_record disagree on {cell:?}");
        table_val
    };

    let base_impl = lookup(&base, impl_cell);
    let base_files = lookup(&base, files_cell);
    assert!(base_impl.is_some(), "crate with an impl has an ImplRows cell");
    assert!(base_files.is_some(), "crate with a file has a Files cell");

    assert_eq!(lookup(&priv_fn, impl_cell), base_impl, "adding a private fn must not move ImplRows");
    assert_eq!(lookup(&priv_fn, files_cell), base_files, "adding a private fn must not move Files");

    assert_ne!(lookup(&with_impl, impl_cell), base_impl, "adding an impl must move ImplRows");
    assert_eq!(lookup(&with_impl, files_cell), base_files, "adding an impl must not move Files");

    assert_eq!(lookup(&with_file, impl_cell), base_impl, "adding an empty module file must not move ImplRows");
    assert_ne!(lookup(&with_file, files_cell), base_files, "adding a module file must move Files");
}

#[test]
fn extensions_and_cfg_atoms_cells_stand_on_private_fn_and_move_on_own_change() {
    const BASE: &str = "#![no_core]\npub fn a() {}\n";
    const WITH_PRIV_FN: &str = "#![no_core]\npub fn a() {}\nfn __priv() {}\n";
    const WITH_UNIQ: &str = "#![no_core]\n#![blaze::uniq]\npub fn a() {}\n";
    const WITH_UNIQ_RECORDS: &str = "#![no_core]\n#![blaze::uniq]\n#![blaze::records]\npub fn a() {}\n";

    let lookup = |src: &'static str, cell_of: &dyn Fn([u8; 8]) -> NameCell| -> Option<ContentId> {
        let solved = super::items_record::solve_one(src, None);
        let bump = super::SyncFrame::leaked();
        let g = bump.alloc();
        let names_bytes = aligned(&encode(solved, g).expect("encode names"), bump);
        let items_bytes = aligned(&items_encode(solved, g).expect("encode items"), bump);
        let wholes = [(Section::Items, items_bytes)];
        let tbl = crate::names_cells::crate_table(solved, names_bytes, items_bytes, &wholes, bump).expect("crate_table");
        let cell = cell_of(solved.key.prefix());
        let table_val = seen_in(&tbl, cell.key()).expect("seen_in");
        let record = NamesRecord::open(Verified::check(names_bytes).expect("verify names"));
        let mut items = crate::items_record::ItemsRecord::open(items_bytes, g).expect("open items");
        items.use_solved_texts(solved, bump);
        let read = |of: TextOf<'_>| -> Option<&str> {
            match of {
                TextOf::File { index: 0, .. } => Some(src),
                _ => None,
            }
        };
        let rec_val = seen_by_record(&record, Some((&items, &read)), &wholes, cell).expect("seen_by_record");
        assert_eq!(table_val, rec_val, "crate_table and seen_by_record disagree on {cell:?}");
        table_val
    };

    assert_eq!(lookup(BASE, &|krate| NameCell::Extensions { krate }), None, "plain crate has absent Extensions cell");
    assert_eq!(lookup(WITH_PRIV_FN, &|krate| NameCell::Extensions { krate }), None, "adding private fn keeps Extensions absent");
    let base_cfg = lookup(BASE, &|krate| NameCell::CfgAtoms { krate });
    assert_eq!(lookup(WITH_PRIV_FN, &|krate| NameCell::CfgAtoms { krate }), base_cfg, "adding private fn must not move CfgAtoms");

    let uniq_ext = lookup(WITH_UNIQ, &|krate| NameCell::Extensions { krate });
    assert!(uniq_ext.is_some(), "#![blaze::uniq] defines Extensions cell");
    let both_ext = lookup(WITH_UNIQ_RECORDS, &|krate| NameCell::Extensions { krate });
    assert!(both_ext.is_some() && both_ext != uniq_ext, "adding #![blaze::records] moves Extensions cell");
}
