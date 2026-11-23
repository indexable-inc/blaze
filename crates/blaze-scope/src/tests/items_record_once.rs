//! An items record's strings are verified ONCE, at open (lane RP4, floor B, rule 74): a row
//! read checks no UTF-8 again, and a bad byte in any row's string (an item's name, a foreign
//! block's ABI) is refused by [`ItemsRecord::open`] itself, typed, never at the read.

use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Alloc, Erased};

use super::items_record::{one_file, solve_one};
use crate::items_record::{ItemsRecord, ItemsRecordError, encode};
use crate::resolve::aligned;

const TEXT: &str = r#"#![no_core]
pub struct Zqname;
pub fn zq_other() {}
pub mod zq_mod { pub struct Zqinner; }
extern "C-unwind" { fn zq_ext(); }
"#;

/// The crate's items record's bytes, as written, aligned in `g`. The solve is the items record
/// tests' own ([`solve_one`]).
fn record_bytes(g: blaze_own::Bump<'_>) -> &[u8] {
    let names = solve_one(TEXT, None);
    assert!(names.notes.is_empty(), "notes {:?}", names.notes);
    aligned(&encode(names, g.alloc()).unwrap_or_else(|e| panic!("items encode: {e:?}")), g)
}

/// The test's allocator: a frame, named.
fn frame() -> Frame {
    Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"))
}

/// Where `needle` is in `bytes`: it must be there exactly once (the writer keeps each string
/// once), so planting a byte there corrupts that one string.
fn only_place(bytes: &[u8], needle: &[u8]) -> usize {
    let mut at = None;
    for (i, w) in bytes.windows(needle.len()).enumerate() {
        if w == needle {
            assert!(at.is_none(), "{:?} is in the record twice", core::str::from_utf8(needle));
            at = Some(i);
        }
    }
    at.unwrap_or_else(|| panic!("{:?} is not in the record", core::str::from_utf8(needle)))
}

/// Expected line: `test tests::items_record_once::a_row_read_checks_no_string_again ... ok`,
/// printing `ITEMS ONCE: rows read N, strings validated at open 1, at read 0 (floor 0)`.
#[test]
fn a_row_read_checks_no_string_again() {
    let frame = frame();
    let erased = Erased::new(&frame);
    let g = erased.bump();
    let record = one_file(record_bytes(g), TEXT, g);
    let (mut rows, mut named, mut foreign) = (0u64, 0u64, 0u64);
    for _ in 0..100 {
        for i in 0..record.items() {
            let row = record.item_at(i).unwrap_or_else(|e| panic!("row {i}: {e:?}")).unwrap_or_else(|| panic!("row {i} absent"));
            // The same row by key: the search and the decode, as `item` is asked.
            let by_key = record.item(row.key).unwrap_or_else(|e| panic!("row {i} by key: {e:?}"));
            assert_eq!(by_key, Some(row), "row {i}: by index and by key differ");
            rows += 2;
            named += u64::from(row.name.is_some());
            if let Some(abi) = row.foreign {
                assert_eq!(abi, "C-unwind");
                foreign += 1;
            }
        }
        for i in 0..record.modules() {
            record.module_at(i).unwrap_or_else(|e| panic!("module row {i}: {e:?}"));
            rows += 1;
        }
    }
    let checks = record.decoded().string_checks;
    std::println!("ITEMS ONCE: rows read {rows}, strings validated at open {checks}, at read {} (floor 0)", checks - 1);
    assert!(named > 0 && foreign > 0, "the fixture read no name ({named}) or no ABI ({foreign})");
    assert_eq!(checks, 1, "the strings were checked again after open");
}

/// Expected line: `test tests::items_record_once::a_bad_byte_in_a_name_or_an_abi_is_refused_at_open ... ok`.
/// A 0xFF planted in an item's name, then in a foreign block's ABI: each record is refused by
/// `open`, as [`ItemsRecordError::Utf8`] naming the strings; the unplanted record opens (the
/// control: the refusal is the planted byte's).
#[test]
fn a_bad_byte_in_a_name_or_an_abi_is_refused_at_open() {
    let frame = frame();
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let g = bump.alloc();
    let bytes = record_bytes(bump);
    assert!(ItemsRecord::open(bytes, g).is_ok(), "the unplanted record does not open");
    for needle in [&b"Zqname"[..], &b"C-unwind"[..]] {
        let at = only_place(bytes, needle);
        let mut raw: AVec<'_, u8> = AVec::new_in(g);
        raw.extend_from_slice(bytes);
        raw[at] = 0xff;
        let planted = aligned(&raw, bump);
        match ItemsRecord::open(planted, g) {
            Err(ItemsRecordError::Utf8 { what: "the record's strings", .. }) => {}
            Err(other) => panic!("{:?}: refused at open, but as {other:?}", core::str::from_utf8(needle)),
            Ok(_) => panic!("{:?}: a bad byte in it opened: it would be refused only at a read", core::str::from_utf8(needle)),
        }
    }
}
