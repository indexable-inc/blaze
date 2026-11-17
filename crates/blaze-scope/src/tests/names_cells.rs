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
use crate::names_cells::{Log, Logged, NameCell, Section, seen_by_record, seen_in, table};
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

static BEFORE: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1)]]);
static AFTER: Texts = Texts(&[&[("/k0/lib.rs", K0_ADDED)], &[("/k1/lib.rs", K1)]]);

struct Loaded(Vec<&'static [u8]>);

impl<'a> LoadNames<'a> for Loaded {
    fn load(&self, krate: usize) -> Option<&'a [u8]> {
        self.0.get(krate).copied()
    }
}

/// One side of the edit: each crate's names record, opened, and its cell table; and the
/// table with the items record's cells and a `Whole(Items)` over its bytes.
struct Side {
    bytes: Vec<&'static [u8]>,
    keys: Vec<CrateItemKey>,
    tables: Vec<Vec<u8>>,
    with_items: Vec<Vec<u8>>,
    items: Vec<&'static [u8]>,
}

impl Side {
    fn solve(texts: &'static Texts) -> Side {
        let g = blaze_own::testing::heap().alloc();
        let inputs = [input("k0", "/k0/lib.rs", &[]), input("k1", "/k1/lib.rs", &[("k0", 0)])];
        let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("solve: {e:?}"));
        let mut side = Side { bytes: Vec::new(), keys: Vec::new(), tables: Vec::new(), with_items: Vec::new(), items: Vec::new() };
        for (i, s) in solved.iter().enumerate() {
            assert!(s.names.notes.is_empty(), "crate {i}: notes {:?}", s.names.notes);
            let bytes = aligned(&encode(s.names, g).unwrap_or_else(|e| panic!("encode: {e:?}")), g);
            let record = side.record_of(bytes);
            side.tables.push(table(&record, None, &[], g).unwrap_or_else(|e| panic!("crate {i}'s cell table: {e:?}")).to_vec());
            let items_bytes = aligned(&items_encode(s.names, g).unwrap_or_else(|e| panic!("items encode: {e:?}")), g);
            let items = ItemsRecord::open(items_bytes, g).unwrap_or_else(|e| panic!("crate {i}'s items record: {e:?}"));
            side.items.push(items_bytes);
            let wholes = [(Section::Items, items_bytes)];
            let text: &'static str = texts.0[i][0].1;
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

    fn record(&self, krate: usize) -> NamesRecord<'static> {
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

fn family() -> Vec<(PathKind, Vec<&'static str>)> {
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
        let g = blaze_own::testing::heap().alloc();
        let items = ItemsRecord::open(after.items[0], g).unwrap_or_else(|e| panic!("{e:?}"));
        (0..items.items()).filter_map(|i| items.item_at(i).ok().flatten()).find(|row| row.kind == ItemKind::Module && row.name == Some("a")).map(|row| row.key)
    };
    let a_item = a_item.unwrap_or_else(|| panic!("k0's items record has no row of `mod a`"));
    // The row of `mod a` holds its head only (`module_head`): the fn added inside it moves the
    // module's own cells, never the parent's cell of that row.
    let mut floor = std::vec![added, NameCell::Item { key: z }.key(), NameCell::Whole { krate: before.keys[0].prefix(), section: Section::Items }.key()];
    floor.sort_unstable();
    let items_moved = moved(&before.with_items[0], &after.with_items[0]);
    std::println!("NAMES CELLS: with the items record, {} cell(s) of k0 moved (floor 3: the binding, the item row, the whole section)", items_moved.len());
    assert!(!items_moved.contains(&NameCell::Item { key: a_item }.key()), "the row of `mod a` moved with an item added inside it");
    let mut candidates: Vec<(std::string::String, [u8; 16])> = std::vec![("Item(added)".into(), NameCell::Item { key: z }.key())];
    let g = blaze_own::testing::heap().alloc();
    let items_after = ItemsRecord::open(after.items[0], g).unwrap_or_else(|e| panic!("{e:?}"));
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
