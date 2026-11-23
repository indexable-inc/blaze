//! The items record at item grain ([`crate::items_record::grain`]; drive.md, "End state",
//! piece 4): an edit to one item's text voids that item's row alone. A crate of one file with
//! `fn f(a: u8) -> u8 { a }` and `fn g() -> u8 { 1 }` is solved once and its record written;
//! then the file is read back as four texts:
//!
//! 1. the same text: written twice, equal bytes; both rows answered as written (`Same`);
//! 2. `g`'s body edited: `f`'s row fresh, its body unchanged, its text read at today's place;
//!    `g`'s row fresh too (its signature did not change), only its body digest differs;
//! 3. `f`'s signature edited (a parameter added: a type-level edit `Surface` leaves out): `f` is
//!    fresh with its body changed, read at today's place (the row holds no head: D114 ruling);
//!    `g`, moved, is fresh with its body unchanged;
//! 4. a blank line and a comment inserted above `f`: nothing is stale, and each row's text is
//!    found at its new place (rule 71: no row holds a position from the file's start).
//!
//! A file-grain record (any edit voids every row of its file) fails 2, 3 and 4; a row that
//! compares a head fails 3 (`f` stale); a chain that holds a file offset fails 4.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-scope test an_edit_voids_only_the_items_it_touches`:
//! `test tests::items_grain::an_edit_voids_only_the_items_it_touches ... ok`.

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::items_record::{ItemNow, ItemRow, ItemsRecord, LoadItems, RecordItems, encode};
use crate::resolve::{At, aligned};
use crate::task::Serial;

const WAIT: core::time::Duration = core::time::Duration::from_secs(60);
const PATH: &str = "/k0/lib.rs";
const BEFORE: &str = "#![no_core]\nfn f(a: u8) -> u8 { a }\nfn g() -> u8 { 1 }\n";
const G_BODY: &str = "#![no_core]\nfn f(a: u8) -> u8 { a }\nfn g() -> u8 { 2 }\n";
const F_SIGNATURE: &str = "#![no_core]\nfn f(a: u8, b: u8) -> u8 { a }\nfn g() -> u8 { 1 }\n";
const MOVED: &str = "#![no_core]\n\n// a note\nfn f(a: u8) -> u8 { a }\nfn g() -> u8 { 1 }\n";
/// `MOVED` minus `BEFORE`: "\n// a note\n".
const INSERTED: u32 = 11;
const F_TEXT: &str = "fn f(a: u8) -> u8 { a }";
const G_TEXT: &str = "fn g() -> u8 { 1 }";

static FILES: [(&str, &str); 1] = [(PATH, BEFORE)];
static CRATES: [&[(&str, &str)]; 1] = [&FILES];
static TEXTS: Texts = Texts(&CRATES);

/// The crate's one file as it is now; its record as written.
#[derive(Clone, Copy)]
struct Now {
    record: &'static [u8],
    text: &'static str,
}

impl LoadItems<'static> for Now {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.record)
    }

    fn file(&self, krate: usize, path: crate::package_path::PackagePath<'_>) -> Option<&'static str> {
        // The crate's package is at `/` (entry.rs `input`): its record holds `k0/lib.rs`.
        (krate == 0 && path.in_package("/", Alloc::global()).as_deref() == Some(PATH)).then_some(self.text)
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<&'static str> {
        None
    }
}

/// `key`'s row against `text`, through a reader that has checked nothing yet.
fn now(before: &ItemsRecord<'_>, record: &'static [u8], krate: CrateItemKey, text: &'static str, key: ItemKey) -> ItemNow<'static> {
    let read = RecordItems::new(&[krate], Now { record, text }, super::SyncFrame::leaked());
    read.item_now_since(key, before).unwrap_or_else(|e| panic!("item {key:?} against today's text: {e:?}")).unwrap_or_else(|| panic!("no row for item {key:?}"))
}

fn key_of(record: &ItemsRecord<'_>, name: &str) -> ItemKey {
    for i in 0..record.items() {
        if let Ok(Some(row)) = record.item_at(i) {
            if row.name == Some(name) {
                return row.key;
            }
        }
    }
    panic!("no item row named `{name}`")
}

fn text_of<'t>(text: &'t str, row: &ItemRow<'_>) -> &'t str {
    let span = row.text.unwrap_or_else(|| panic!("row {:?} has no place", row.name));
    assert!(span.at == At::File(0), "row {:?} is not in the file: {:?}", row.name, span.at);
    &text[span.start as usize..(span.start + span.len) as usize]
}

/// A fresh row read at today's place: its text is `want` and its body changed as `body`.
fn fresh(what: &str, got: ItemNow<'static>, was: &ItemRow<'static>, want: &str, body: bool) -> ItemRow<'static> {
    let ItemNow::Fresh { row, text, body_changed } = got else { panic!("{what}: not fresh at item grain: {got:?}") };
    assert!(body_changed == body, "{what}: body changed {body_changed}, want {body}");
    assert!(row.key == was.key && row.kind == was.kind && row.name == was.name && row.module == was.module, "{what}: the row's facts moved: {row:?} against {was:?}");
    assert!(text_of(text, &row) == want, "{what}: read `{}` at today's place, want `{want}`", text_of(text, &row));
    row
}

/// The property of the module docs, steps 1 to 4.
#[test]
fn an_edit_voids_only_the_items_it_touches() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", PATH, &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let names = solved[0].names;
    let a = encode(names, g).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let b = encode(names, g).unwrap_or_else(|e| panic!("encode again: {e:?}"));
    assert!(a == b, "the record encodes to different bytes twice");
    let bytes = aligned(&a, bump);
    let original = RecordItems::new(&[names.key], Now { record: bytes, text: BEFORE }, bump);
    let record = original.record(0).expect("the original items record");
    let (f, gk) = (key_of(record, "f"), key_of(record, "g"));
    let was_f = record.item(f).ok().flatten().unwrap_or_else(|| panic!("no row for `f`"));
    let was_g = record.item(gk).ok().flatten().unwrap_or_else(|| panic!("no row for `g`"));
    let pieces_g = record.pieces(gk).ok().flatten().unwrap_or_else(|| panic!("no grain row for `g`"));

    // 1. The same text: every row as written.
    for (key, was, want) in [(f, &was_f, F_TEXT), (gk, &was_g, G_TEXT)] {
        match now(record, bytes, names.key, BEFORE, key) {
            ItemNow::Same(row) => {
                assert!(row == *was, "an unchanged row reads back otherwise: {row:?} against {was:?}");
                assert!(text_of(BEFORE, &row) == want, "the row's text is `{}`, want `{want}`", text_of(BEFORE, &row));
            }
            other => panic!("an unchanged file's row is not answered as written: {other:?}"),
        }
    }

    // 2. `g`'s body edited.
    fresh("`f` after `g`'s body edit", now(record, bytes, names.key, G_BODY, f), &was_f, F_TEXT, false);
    fresh("`g` after its body edit", now(record, bytes, names.key, G_BODY, gk), &was_g, "fn g() -> u8 { 2 }", true);
    let outline = blaze_file_items::outline(G_BODY, record.edition(), g);
    let g_now = outline.item(was_g.outline);
    assert!(g_now.hash != pieces_g.body, "`g`'s body piece did not change with its body");

    // 3. `f`'s signature edited: `f` is fresh at today's place, its body changed (the row
    // holds no head; the edit moves `f`'s `Signature`, not its row).
    fresh("`f` after its signature edit", now(record, bytes, names.key, F_SIGNATURE, f), &was_f, "fn f(a: u8, b: u8) -> u8 { a }", true);
    fresh("`g` after `f`'s signature edit", now(record, bytes, names.key, F_SIGNATURE, gk), &was_g, G_TEXT, false);

    // 4. Lines inserted above `f`: nothing stale, each row at its new place.
    let moved = fresh("`f` moved down", now(record, bytes, names.key, MOVED, f), &was_f, F_TEXT, false);
    let (old, new) = (was_f.text.map(|s| s.start), moved.text.map(|s| s.start));
    assert!(new == old.map(|s| s + INSERTED), "`f` moved from {old:?} to {new:?}, want {INSERTED} bytes on");
    fresh("`g` moved down", now(record, bytes, names.key, MOVED, gk), &was_g, G_TEXT, false);
}
