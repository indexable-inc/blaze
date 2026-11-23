//! The items record holds nothing its read list does not cover (D114 rulings, "Signatures";
//! D116's rule). A crate's `CrateScope` reads each file's `Surface` (blaze-file-items
//! names_digest.rs: no parameter, return or field type, no generics or bounds), so every FACT
//! of an item row, a member row and a grain row must be equal for two texts that differ only in
//! a parameter's type, a return type, a trait member's return type and a generic bound. Before
//! the ruling the grain row held each entry's head digest (`head_of`, the signature's bytes): a
//! type edit left the scope standing with a row that front2 refused as stale, and nothing
//! remade it.
//!
//! What is compared ([`facts`]): per item row its key, kind, module, outline entry, name,
//! flags (body holds items, constructor) and impl row; per member row its key, name, outline
//! entry, kind, flag and expansion; per grain row its chain. Positions (a row's text span, an
//! impl header's place), the file's text digest and the grain row's whole-item digest are
//! proofs a reader checks against today's text (a moved one is refused or read at today's
//! place, `ItemNow::Fresh`), not facts, and are left out by name.
//!
//! The planted bug ([`facts_with_head`]): the same facts with each entry's `head_of` added, the
//! row as it was before the ruling. The test asserts the oracle catches it on every variant, so
//! the comparison is able to see a row fact that reads a type.
//!
//! Then each variant's edited items, read through the BASE record against the variant's text,
//! are `ItemNow::Fresh` (no head is compared any more), at today's place.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-scope test items_head_free`: two tests ok,
//! `test tests::items_head_free::a_type_only_edit_leaves_every_row_fact_equal ... ok` and
//! `test tests::items_head_free::a_type_only_edit_is_fresh_through_the_old_record ... ok`.

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::items_record::{ItemNow, ItemsRecord, LoadItems, RecordItems, encode};
use crate::resolve::aligned;
use crate::task::Serial;

const WAIT: core::time::Duration = core::time::Duration::from_secs(60);
const PATH: &str = "/k0/lib.rs";

/// The base text: a trait with a member, a fn with a parameter and a return type, a generic fn
/// with a bound.
const BASE: &str = "#![no_core]\npub trait A { fn m(&self) -> u8; }\npub trait B {}\npub fn f(a: u8) -> u8 { a }\npub fn h<X: A>(_x: X) {}\n";
/// A parameter's type edited.
const PARAM: &str = "#![no_core]\npub trait A { fn m(&self) -> u8; }\npub trait B {}\npub fn f(a: u16) -> u8 { a }\npub fn h<X: A>(_x: X) {}\n";
/// A return type edited, of a fn and of a trait's member.
const RET: &str = "#![no_core]\npub trait A { fn m(&self) -> u16; }\npub trait B {}\npub fn f(a: u8) -> u16 { a }\npub fn h<X: A>(_x: X) {}\n";
/// A generic bound edited.
const BOUND: &str = "#![no_core]\npub trait A { fn m(&self) -> u8; }\npub trait B {}\npub fn f(a: u8) -> u8 { a }\npub fn h<X: B>(_x: X) {}\n";

static BASE_FILES: [(&str, &str); 1] = [(PATH, BASE)];
static BASE_CRATES: [&[(&str, &str)]; 1] = [&BASE_FILES];
static PARAM_FILES: [(&str, &str); 1] = [(PATH, PARAM)];
static PARAM_CRATES: [&[(&str, &str)]; 1] = [&PARAM_FILES];
static RET_FILES: [(&str, &str); 1] = [(PATH, RET)];
static RET_CRATES: [&[(&str, &str)]; 1] = [&RET_FILES];
static BOUND_FILES: [(&str, &str); 1] = [(PATH, BOUND)];
static BOUND_CRATES: [&[(&str, &str)]; 1] = [&BOUND_FILES];

/// Each variant: its words, its texts as the solver reads them, its text, and the names of
/// the items its edit touched.
static VARIANTS: [(&str, Texts, &str, &[&str]); 3] = [
    ("a parameter's type", Texts(&PARAM_CRATES), PARAM, &["f"]),
    ("a return type", Texts(&RET_CRATES), RET, &["f", "A"]),
    ("a generic bound", Texts(&BOUND_CRATES), BOUND, &["h"]),
];

/// One fact of a record, as compared: every field spelled out, so a new fact is added here
/// by name.
#[derive(Debug, PartialEq, Eq)]
enum Fact<'a> {
    Item { key: ItemKey, kind: u8, module: ItemKey, outline: u32, name: Option<&'a str>, body_holds_items: bool, ctor: bool, impl_row: Option<u32> },
    Member { owner: ItemKey, key: ItemKey, name: Option<&'a str>, outline: u32, kind: u8, body_holds_items: bool, written_by: Option<ItemKey> },
    Chain { key: ItemKey, chain: blaze_id::ContentId },
    /// The planted bug's fact: an entry's head digest, as the grain row held it.
    Head { key: ItemKey, head: blaze_id::ContentId },
}

/// The crate of one file solved over `texts`, its items record encoded and opened.
fn record_of(texts: &'static Texts) -> (ItemsRecord<'static>, &'static [u8], CrateItemKey) {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", PATH, &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let names = solved[0].names;
    let bytes = encode(names, g).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let bytes: &'static [u8] = aligned(&bytes, bump);
    (super::items_record::one_file(bytes, names.files[0].text, bump), bytes, names.key)
}

/// The record's facts (module docs), in row order.
fn facts<'a>(record: &ItemsRecord<'a>) -> std::vec::Vec<Fact<'a>> {
    let mut out = std::vec::Vec::new();
    for i in 0..record.items() {
        let row = record.item_at(i).unwrap_or_else(|e| panic!("item row {i}: {e:?}")).unwrap_or_else(|| panic!("no item row {i}"));
        out.push(Fact::Item {
            key: row.key,
            kind: row.kind as u8,
            module: row.module,
            outline: row.outline.0,
            name: row.name,
            body_holds_items: row.body_holds_items,
            ctor: row.ctor,
            impl_row: row.impl_row,
        });
        let pieces = record.pieces_at(i).unwrap_or_else(|e| panic!("grain row {i}: {e:?}"));
        out.push(Fact::Chain { key: row.key, chain: pieces.chain });
        if let Some(members) = record.members(row.key).unwrap_or_else(|e| panic!("members of row {i}: {e:?}")) {
            for m in members.iter() {
                let m = m.unwrap_or_else(|e| panic!("a member row of row {i}: {e:?}"));
                out.push(Fact::Member {
                    owner: row.key,
                    key: m.key,
                    name: m.name,
                    outline: m.outline.0,
                    kind: m.kind as u8,
                    body_holds_items: m.body_holds_items,
                    written_by: m.written_by,
                });
            }
        }
    }
    out
}

/// THE PLANTED BUG: [`facts`] with each top-level entry's head digest (`head_of` of its entry
/// in `text`'s outline), the fact the grain row held before the ruling.
fn facts_with_head<'a>(record: &ItemsRecord<'a>, text: &str) -> std::vec::Vec<Fact<'a>> {
    let mut out = facts(record);
    let outline = blaze_file_items::outline(text, record.edition(), Alloc::global());
    for i in 0..record.items() {
        let row = record.item_at(i).ok().flatten().unwrap_or_else(|| panic!("no item row {i}"));
        if row.text.is_none() {
            continue;
        }
        let Some(item) = outline.items().get(row.outline.0 as usize) else { continue };
        out.push(Fact::Head { key: row.key, head: blaze_file_items::head_of(item) });
    }
    out
}

/// Every row fact is equal across a type-only edit, and the planted head fact is caught.
#[test]
fn a_type_only_edit_leaves_every_row_fact_equal() {
    static BASE_TEXTS: Texts = Texts(&BASE_CRATES);
    let (base, _, _) = record_of(&BASE_TEXTS);
    let base_facts = facts(&base);
    assert!(base_facts.iter().any(|f| matches!(f, Fact::Member { name: Some("m"), .. })), "the base record lists `A::m`: {base_facts:?}");
    for (what, texts, text, _) in VARIANTS.iter() {
        let (edited, _, _) = record_of(texts);
        let edited_facts = facts(&edited);
        assert!(edited_facts == base_facts, "{what}: a row fact moved with a type-only edit (the record holds something its `Surface` read does not cover):\n{edited_facts:?}\nagainst\n{base_facts:?}");
        // The control: the oracle sees a fact that reads the type.
        assert!(
            facts_with_head(&edited, text) != facts_with_head(&base, BASE),
            "{what}: the planted head fact was not caught: the comparison cannot see a row fact that reads a type"
        );
    }
    std::println!("items record: 3 type-only edits, every row fact equal; planted head fact caught 3 of 3");
}

/// The base record's one file as each variant's text: what a reader of a standing scope sees.
#[derive(Clone, Copy)]
struct Today {
    record: &'static [u8],
    text: &'static str,
}

impl LoadItems<'static> for Today {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.record)
    }

    fn file(&self, krate: usize, path: crate::package_path::PackagePath<'_>) -> Option<&'static str> {
        (krate == 0 && path.in_package("/", Alloc::global()).as_deref() == Some(PATH)).then_some(self.text)
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<&'static str> {
        None
    }
}

/// Each variant's edited items, read through the BASE record against the variant's text, are
/// fresh at today's place: the head is no fact of the row, so a type edit voids nothing.
#[test]
fn a_type_only_edit_is_fresh_through_the_old_record() {
    static BASE_TEXTS: Texts = Texts(&BASE_CRATES);
    let (base, bytes, krate) = record_of(&BASE_TEXTS);
    for (what, _, text, touched) in VARIANTS.iter() {
        let read = RecordItems::new(&[krate], Today { record: bytes, text }, super::SyncFrame::leaked());
        for name in touched.iter() {
            let key = (0..base.items())
                .find_map(|i| base.item_at(i).ok().flatten().filter(|row| row.name == Some(*name)).map(|row| row.key))
                .unwrap_or_else(|| panic!("no item row named `{name}`"));
            match read.item_now_since(key, &base) {
                Ok(Some(ItemNow::Fresh { row, text: today, body_changed: true })) => {
                    let span = row.text.unwrap_or_else(|| panic!("{what}: `{name}` fresh with no place"));
                    let seen = &today[span.start as usize..(span.start + span.len) as usize];
                    assert!(seen.contains(&**name), "{what}: `{name}` read at today's place gave `{seen}`");
                }
                other => panic!("{what}: `{name}` is not fresh with its text changed through the old record: {other:?}"),
            }
        }
    }
    std::println!("items record: every type-edited item fresh through the old record, 3 variants");
}
