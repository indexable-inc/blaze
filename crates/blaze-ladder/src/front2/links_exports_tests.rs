//! links.rs's exports (native-W9-names): each export carries the item it names (its
//! `blaze_id::ItemKey`, the key nested_exports.rs's `NestedExport` carries), the list is sorted
//! by name, and two exports of one name in one crate are refused by name, never deduplicated.
//!
//! A child of `links` (its `mod` line is in links.rs, `#[cfg(test)]`), so it reads `Gather`.

use blaze_id::{ContentId, ItemKey};
use blaze_own::{Bump, Erased};
use blaze_surface::{Edition, ItemIndex};

use super::{CrateExport, CrateLinks, Entry, Gather, KeyFor, LinksError, TextOf};

const MAC: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];

/// A unit test's key for entry `at` of its one text (a unit test has no `Names`): the index in
/// bytes 0..4, the rest zero. Distinct entries give distinct keys, in index order.
pub(super) fn index_key(at: ItemIndex) -> ItemKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&at.0.to_le_bytes());
    ItemKey(ContentId(bytes))
}

/// The facts of one file `text` as the only text of crate `t`, on macOS.
fn links_of<'a>(text: &'a str, bump: Bump<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
    let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
    let mut gather = Gather::new("t", Edition::E2021, MAC, &[], bump);
    gather.text(TextOf::File("src/lib.rs"), text, &outline, &|at| Entry::Keyed(index_key(at)))?;
    gather.finish()
}

/// Rust Reference, "The no_mangle attribute" ([abi.no_mangle]): "the symbol for the item is the
/// item's identifier", and "The export_name attribute" ([abi.export_name]) names the symbol
/// itself; [abi.no_mangle.unsafe]: an unmangled symbol "may collide with another symbol with the
/// same name". Two items of one crate exporting one symbol name are an error (the Reference text
/// is not on the toolchain shelf; the sections are named). So two `#[no_mangle] fn f` (one at
/// the top, one in `mod m`) are refused naming both items and the name, and never merged into
/// one; a lone export keeps the item it names, the list sorted by name.
///
/// Prints `links: two exporters of `f` refused (TwoExports), each export keeps its item`, then
/// `test front2::links::exports_tests::two_exporters_of_one_name_refused ... ok`.
/// Without the piece (the old `sort_unstable` + `dedup`): `exports == ["f"]`, no error, and the
/// first `expect_err` panics with "two exporters of `f` were merged".
#[test]
fn two_exporters_of_one_name_refused() {
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let erased = Erased::new(&frame);
    let bump = erased.bump();

    // Entries in outline order: 0 `fn f`, 1 `mod m`, 2 `m::f`.
    let two = "#[no_mangle] pub extern \"C\" fn f() {}\n\
               mod m { #[unsafe(no_mangle)] pub extern \"C\" fn f() {} }\n";
    let refused = links_of(two, bump).err().unwrap_or_else(|| panic!("two exporters of `f` were merged"));
    assert_eq!(
        refused,
        LinksError::TwoExports {
            krate: "t",
            first: CrateExport { name: "f", item: index_key(ItemIndex(0)), ident: "f" },
            second: CrateExport { name: "f", item: index_key(ItemIndex(2)), ident: "f" },
        }
    );

    // `export_name` and `no_mangle` naming one symbol from two items: refused the same way.
    let named = "#[export_name = \"f\"] pub static S: u8 = 0;\n#[no_mangle] pub extern \"C\" fn f() {}\n";
    assert!(matches!(links_of(named, bump), Err(LinksError::TwoExports { first: CrateExport { name: "f", .. }, .. })));

    // One export of each name: each keeps its item; sorted by name, not by place.
    let one = "#[export_name = \"g\"] pub static S: u8 = 0;\n#[no_mangle] pub extern \"C\" fn f() {}\n";
    let links = links_of(one, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
    assert_eq!(
        links.exports.as_slice(),
        [
            CrateExport { name: "f", item: index_key(ItemIndex(1)), ident: "f" },
            CrateExport { name: "g", item: index_key(ItemIndex(0)), ident: "S" },
        ]
    );

    // An exported entry the names source gives no key is refused by name, never passed over.
    let outline = blaze_file_items::outline(one, Edition::E2021, bump.alloc());
    let mut gather = Gather::new("t", Edition::E2021, MAC, &[], bump);
    assert_eq!(
        gather.text(TextOf::File("src/lib.rs"), one, &outline, &|_| Entry::Unkeyed).err(),
        Some(LinksError::Unkeyed { krate: "t", text: TextOf::File("src/lib.rs"), at: ItemIndex(0), line: 1, item: "S", needs: KeyFor::Export })
    );

    println!("links: two exporters of `f` refused (TwoExports), each export keeps its item");
}
