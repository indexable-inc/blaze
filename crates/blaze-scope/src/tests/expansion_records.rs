//! B3 (runtime.md plan row B3): macro expansions as their own records, keyed by content
//! ([`crate::expansion_records`]).
//!
//! One `macro_rules!` called with equal input in 50 modules: one expansion record, 50 directory
//! rows, the directory a small fraction of the texts section it replaces. Every text and every
//! origin answered from a directory row and its record equals the one the solve made. The items
//! record's expansion rows are the directory's set in its order, each row's text id the text its
//! record holds (what `NameCell::Expansion` digests, so a reader's cell covers its record). One
//! call's input edited moves that call's record only.
//!
//! What may differ from before in a crate's `CrateScope` payload (blaze-cli names.rs
//! `scope_record`, once it writes the directory): the expansions section (the directory replaces
//! the texts) and, in the cell table, the one `Whole { section: Expansions }` row's digest (it is
//! the section's bytes'). The names and items records are written by `names_record::encode` and
//! `items_record::encode` from the solve alone, which this piece does not touch: byte-equal by
//! construction, and every names_groups fixture reads them as before.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, ItemKey};
use blaze_own::Alloc;

use super::entry::{Texts, input};
use crate::expansion_origins::OriginsView;
use crate::expansion_records::{self, DIRECTORY_ROW, Directory, ExpansionRecord, Split, record_key};
use crate::graph::Sysroot;
use crate::items_record::{ItemsRecord, TextOf, expansion_id};
use crate::names_cells::{NameCell, Section, crate_table, seen_by_record, seen_in};
use crate::names_record::{NamesRecord, Verified};
use crate::resolve::{CrateNames, aligned, resolve_crates};
use crate::task::Serial;

const MODULES: usize = 50;
const EDITED: usize = 7;

/// A macro whose expansion is long beside a directory row: a struct and two fns.
const MACRO: &str = "macro_rules! many {
    ($t:ident) => {
        pub struct $t { pub a: u8, pub b: u16, pub c: u32 }
        pub fn make() -> $t { $t { a: 1, b: 2, c: 3 } }
        pub fn take(_x: $t) -> u8 { 0 }
    };
}
";

fn leak(s: String) -> &'static str {
    std::boxed::Box::leak(s.into_boxed_str())
}

/// The crate: [`MACRO`] then [`MODULES`] modules each calling it with `Thing`, the module
/// `edited` with `Other`.
fn source(edited: Option<usize>) -> &'static str {
    let mut t = String::from("#![no_core]\n");
    t += MACRO;
    for m in 0..MODULES {
        let ty = if edited == Some(m) { "Other" } else { "Thing" };
        t += &format!("pub mod m{m} {{\n    many!({ty});\n}}\n");
    }
    leak(t)
}

fn solve(text: &'static str, alloc: blaze_own::Bump<'static>) -> &'static CrateNames<'static> {
    let files: &'static [(&'static str, &'static str)] = Vec::leak(std::vec![("/k/lib.rs", text)]);
    let texts: &'static Texts = std::boxed::Box::leak(std::boxed::Box::new(Texts(Vec::leak(std::vec![files]))));
    let crates = [input("k", "/k/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), texts, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let krate = names.krate(0);
    let made = krate.expansions.iter().filter(|(_, e)| e.made.is_some()).count();
    assert_eq!(made, MODULES, "each module's call made its text: notes {:?}", names.notes(0));
    krate
}

fn split_of(names: &CrateNames<'_>, alloc: blaze_own::Bump<'static>) -> (Split<'static>, Directory<'static>) {
    let split = expansion_records::write(names, alloc.alloc()).unwrap_or_else(|e| panic!("the split is written: {e}"));
    let directory = Directory::open(aligned(&split.directory, alloc)).unwrap_or_else(|e| panic!("the directory opens: {e}"));
    (split, directory)
}

/// The record `key` of `split`, opened.
fn record<'s>(split: &'s Split<'_>, key: ContentId) -> ExpansionRecord<'s> {
    let at = split.records.binary_search_by_key(&key, |r| r.key).unwrap_or_else(|_| panic!("the split holds record {key:?}"));
    ExpansionRecord::open(&split.records[at].bytes).unwrap_or_else(|e| panic!("record {key:?} opens: {e}"))
}

/// Rule: 50 calls of one macro with equal input name ONE expansion record, from 50 directory
/// rows; the record's key is its bytes' digest; the directory is a row and a call part a call,
/// and with the one record it is under half the old texts section (each call's text, origins
/// and 48-byte row, blaze-cli names/expansion_texts.rs form 3).
///
/// Expected: "1 passed". Before B3 the section held the text and origins 50 times.
#[test]
fn equal_calls_share_one_expansion_record() {
    let alloc = super::SyncFrame::leaked();
    let names = solve(source(None), alloc);
    let (split, directory) = split_of(names, alloc);
    assert_eq!(directory.len(), MODULES, "a row a call");
    assert_eq!(split.records.len(), 1, "fifty equal calls, one record: {:?}", split.records.iter().map(|r| r.key).collect::<Vec<_>>());
    assert_eq!(directory.records(), 1, "the head counts the distinct records");
    let key = split.records[0].key;
    assert_eq!(record_key(&split.records[0].bytes), key, "the key is the record's bytes' digest");
    let mut call_part = 0usize;
    for i in 0..directory.len() {
        let entry = directory.entry_at(i).unwrap_or_else(|e| panic!("row {i}: {e}"));
        assert_eq!(entry.record, key, "row {i} names the one record");
        assert!(!entry.call_part.is_empty(), "row {i}: a macro_rules! call's text keeps its origins");
        call_part = call_part.max(entry.call_part.len());
    }
    // The call part: 64 fixed bytes and the path `many`, padded to 4.
    assert_eq!(call_part, 64 + 4, "a call part is the fixed words and the path");
    let bound = 16 + MODULES * (DIRECTORY_ROW + 68);
    assert!(split.directory.len() <= bound, "the directory is {} bytes, over {bound}", split.directory.len());
    let old: usize = 16 + names.expansions.iter().filter_map(|(_, e)| e.made).map(|m| 48 + m.text.len() + m.origins.map_or(0, |o| o.len())).sum::<usize>();
    let new = split.directory.len() + split.records[0].bytes.len();
    assert!(new * 2 < old, "the directory and its one record ({new} bytes) are under half the texts section ({old} bytes)");
}

/// Rule: every text and origin a directory row and its record answer is the one the solve
/// made; the items record's expansion rows are the directory's calls in its order, each row's
/// text length the directory's and its text id the digest of the text its record holds; the
/// split is a function of the solve (written twice, equal bytes: rule 25).
#[test]
fn the_directory_and_its_records_answer_as_the_texts_did() {
    let alloc = super::SyncFrame::leaked();
    let names = solve(source(Some(EDITED)), alloc);
    let (split, directory) = split_of(names, alloc);
    let mut made = 0usize;
    for (call, expansion) in names.expansions.iter() {
        let Some(text) = expansion.made else { continue };
        made += 1;
        let entry = directory.entry(*call).unwrap_or_else(|e| panic!("call {call:?}: {e}")).unwrap_or_else(|| panic!("call {call:?} has a row"));
        let held = record(&split, entry.record);
        assert_eq!(entry.text(&held).unwrap_or_else(|e| panic!("{e}")), text.text, "call {call:?}: its text");
        match text.origins {
            Ok(bytes) => {
                let was = OriginsView::open(bytes).unwrap_or_else(|e| panic!("call {call:?}: the solve's origins open: {e}"));
                let now = entry.origins(&held).unwrap_or_else(|e| panic!("call {call:?}: {e}")).unwrap_or_else(|| panic!("call {call:?} keeps origins"));
                assert_eq!((now.text, now.item.0, now.input_start, now.input_len, now.shape, now.implicit, now.macro_name), (was.text, was.item.0, was.input_start, was.input_len, was.shape, was.implicit, was.macro_name), "call {call:?}: its call part");
                assert_eq!(now.column.len(), was.column.len(), "call {call:?}: its column's rows");
                for i in 0..was.column.len() {
                    let (a, b) = (now.column.row(i), was.column.row(i));
                    assert_eq!((a.start, a.end, a.origin, a.mark), (b.start, b.end, b.origin, b.mark), "call {call:?}: column row {i}");
                }
            }
            Err(_) => assert!(entry.origins(&held).unwrap_or_else(|e| panic!("{e}")).is_none(), "call {call:?}: no origins kept, none answered"),
        }
    }
    assert_eq!(made, directory.len(), "a row for each call that made a text, and no other");
    assert_eq!(split.records.len(), 2, "two inputs, two records");

    let items = crate::items_record::encode(names, alloc.alloc()).unwrap_or_else(|e| panic!("the items record: {e:?}"));
    let items = ItemsRecord::open(aligned(&items, alloc), alloc.alloc()).unwrap_or_else(|e| panic!("the items record opens: {e:?}"));
    assert_eq!(items.expansions(), directory.len(), "the items record's expansion rows are the directory's set");
    for i in 0..directory.len() {
        let (row, entry) = (items.expansion_at(i).unwrap_or_else(|| panic!("items row {i}")), directory.entry_at(i).unwrap_or_else(|e| panic!("row {i}: {e}")));
        assert_eq!((row.call, row.len), (entry.call, entry.text_len), "row {i}: one call, one length");
        assert_eq!(row.id, expansion_id(record(&split, entry.record).text), "row {i}: the items row's text id is its record's text's");
    }

    let (again, _) = split_of(names, alloc);
    assert_eq!(&again.directory[..], &split.directory[..], "the directory is a function of the solve");
    assert_eq!(again.records.len(), split.records.len());
    for (a, b) in again.records.iter().zip(split.records.iter()) {
        assert_eq!((a.key, &a.bytes[..]), (b.key, &b.bytes[..]), "the records are a function of the solve");
    }
}

/// Each call's (call key, record key), sorted by call.
fn rows(directory: &Directory<'_>) -> BTreeMap<ItemKey, ContentId> {
    (0..directory.len()).map(|i| directory.entry_at(i).unwrap_or_else(|e| panic!("row {i}: {e}"))).map(|e| (e.call, e.record)).collect()
}

/// Rule: one call's input edited moves that call's record key only: the 49 others name the
/// record they named before, and exactly one record is new (the one the engine's put loop puts;
/// a put of the held record under its held key is "same").
#[test]
fn an_edited_call_moves_its_own_record_only() {
    let alloc = super::SyncFrame::leaked();
    let (before, before_dir) = split_of(solve(source(None), alloc), alloc);
    let (after, after_dir) = split_of(solve(source(Some(EDITED)), alloc), alloc);
    let (was, now) = (rows(&before_dir), rows(&after_dir));
    assert_eq!((was.len(), now.len()), (MODULES, MODULES), "a row a call, before and after");
    let kept = now.iter().filter(|(call, key)| was.get(call) == Some(key)).count();
    assert_eq!(kept, MODULES - 1, "every call but the edited one names the record it named before");
    let new: Vec<ContentId> = after.records.iter().map(|r| r.key).filter(|k| before.records.binary_search_by_key(k, |r| r.key).is_err()).collect();
    assert_eq!(new.len(), 1, "one record to put: {new:?}");
    let moved: Vec<(&ItemKey, &ContentId)> = now.iter().filter(|(call, key)| was.get(call) != Some(key)).collect();
    assert_eq!(moved.len(), 1, "one call moved");
    assert_eq!(*moved[0].1, new[0], "the moved call names the new record");
}

/// The crate's cell table as blaze-cli names.rs `scope_record` writes it (its wholes the names,
/// the items and the directory), the table without the directory, and each call's row.
fn tables(text: &'static str, alloc: blaze_own::Bump<'static>) -> (Vec<u8>, Vec<u8>, BTreeMap<ItemKey, ContentId>) {
    let a = alloc.alloc();
    let names = solve(text, alloc);
    let (split, directory) = split_of(names, alloc);
    let names_bytes = crate::names_record::encode(names, a).unwrap_or_else(|e| panic!("the names record: {e:?}"));
    let items_bytes = crate::items_record::encode(names, a).unwrap_or_else(|e| panic!("the items record: {e:?}"));
    let wholes = [(Section::Names, &names_bytes[..]), (Section::Items, &items_bytes[..]), (Section::Expansions, &split.directory[..])];
    let with = crate_table(names, &names_bytes, &items_bytes, &wholes, alloc).unwrap_or_else(|e| panic!("the cell table: {e:?}"));
    let without = crate_table(names, &names_bytes, &items_bytes, &wholes[..2], alloc).unwrap_or_else(|e| panic!("the cell table without the directory: {e:?}"));
    // The record's own search agrees with the table, over the same wholes.
    let record = NamesRecord::open(Verified::check(aligned(&names_bytes, alloc)).unwrap_or_else(|e| panic!("the names record checks: {e:?}")));
    let items = ItemsRecord::open(aligned(&items_bytes, alloc), a).unwrap_or_else(|e| panic!("the items record opens: {e:?}"));
    let read = |_: TextOf<'_>| -> Option<&'static str> { None };
    for (call, _) in rows(&directory) {
        let cell = NameCell::Expansion { call };
        let in_table = seen_in(&with, cell.key()).unwrap_or_else(|e| panic!("{e:?}"));
        let by_record = seen_by_record(&record, Some((&items, &read)), &wholes, cell).unwrap_or_else(|e| panic!("{cell:?}: {e:?}"));
        assert_eq!(in_table, by_record, "call {call:?}: the table and the record disagree");
    }
    (with.to_vec(), without.to_vec(), rows(&directory))
}

/// Rule (B3 step 4): a call's `NameCell::Expansion` digest covers the KEY of the expansion
/// record its directory row names, so a reader of the call's text or origin column is entered
/// when the record moves (a column can move under an equal text: the items row alone would not
/// see it). Each call's cell is in the table and equals the record's own search
/// (`seen_by_record`); CONTROL: the same table without the directory holds another digest for
/// every call (the key is in it); one call's input edited moves that call's cell alone.
///
/// Expected: "1 passed".
#[test]
fn an_expansion_cell_covers_its_record_key() {
    let alloc = super::SyncFrame::leaked();
    let (before, before_without, calls) = tables(source(None), alloc);
    let (after, _, after_calls) = tables(source(Some(EDITED)), alloc);
    assert_eq!(calls.len(), MODULES, "a row a call");
    let mut moved = 0usize;
    for (call, key) in calls.iter() {
        let cell = NameCell::Expansion { call: *call }.key();
        let was = seen_in(&before, cell).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(was.is_some(), "call {call:?} has its cell");
        assert_ne!(was, seen_in(&before_without, cell).unwrap_or_else(|e| panic!("{e:?}")), "CONTROL: call {call:?}'s cell without the directory is the same digest");
        let now = seen_in(&after, cell).unwrap_or_else(|e| panic!("{e:?}"));
        if after_calls.get(call) == Some(key) {
            assert_eq!(was, now, "call {call:?}: its record did not move, nor its cell");
        } else {
            moved += 1;
            assert_ne!(was, now, "call {call:?}: its record moved and its cell did not");
        }
    }
    assert_eq!(moved, 1, "one call's cell moved");
}

/// Rule: an expansion record holds the expansion alone, never how its macro was NAMED nor which
/// crate called it: `make!(one)` in `a`, where the exported macro is defined (textual scope),
/// and `a::make!(one)` in `b` (by its path) are byte-equal records under one key, while their
/// call parts (the directory's) differ, each naming the path as written. What does tell two
/// records apart is the macro's DEFINITION: a token's mark holds its defining crate's key
/// (blaze-expand origin_column.rs `TextMark`), so the same text defined in another crate is
/// another record (blaze-cli engine_expansion_tests.rs: a crate's lib and its test harness).
///
/// Expected: "1 passed".
#[test]
fn a_call_by_path_and_by_textual_scope_share_one_record() {
    const A: &str = "#![no_core]\n#[macro_export]\nmacro_rules! make {\n    ($n:ident) => {\n        pub fn $n() -> u8 { 1 }\n    };\n}\npub mod m {\n    make!(one);\n}\n";
    const B: &str = "#![no_core]\npub mod m {\n    a::make!(one);\n}\n";
    let alloc = super::SyncFrame::leaked();
    let files: &'static [&'static [(&'static str, &'static str)]] = Vec::leak(std::vec![&[("/b/lib.rs", B)][..], &[("/a/lib.rs", A)][..]]);
    let texts: &'static Texts = std::boxed::Box::leak(std::boxed::Box::new(Texts(files)));
    let crates = [input("b", "/b/lib.rs", &[("a", 1)]), input("a", "/a/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), texts, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let (b, a) = (names.krate(0), names.krate(1));
    for (krate, at) in [(b, 0usize), (a, 1)] {
        let made = krate.expansions.iter().filter(|(_, e)| e.made.is_some()).count();
        assert_eq!(made, 1, "`{}`'s one call made its text: notes {:?}", krate.name, names.notes(at));
    }
    let (split_a, dir_a) = split_of(a, alloc);
    let (split_b, dir_b) = split_of(b, alloc);
    assert_eq!((split_a.records.len(), split_b.records.len()), (1, 1), "a record a crate");
    assert_eq!(split_a.records[0].key, split_b.records[0].key, "the two forms of the call are two records");
    assert_eq!(&split_a.records[0].bytes[..], &split_b.records[0].bytes[..], "equal keys, other bytes");
    let (row_a, row_b) = (dir_a.entry_at(0).unwrap_or_else(|e| panic!("a's row: {e}")), dir_b.entry_at(0).unwrap_or_else(|e| panic!("b's row: {e}")));
    let view = |part: &'static [u8]| crate::expansion_origins::CallView::open(part).unwrap_or_else(|e| panic!("a call part opens: {e}"));
    let (call_a, call_b) = (view(row_a.call_part), view(row_b.call_part));
    assert_eq!((call_a.macro_name, call_b.macro_name), ("make", "a::make"), "each call part names the path as written");
    assert_ne!(row_a.call_part, row_b.call_part, "the call parts are equal: the path is not the call's");
}
