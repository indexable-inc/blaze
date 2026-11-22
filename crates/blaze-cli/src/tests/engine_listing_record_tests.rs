//! A CRATE'S LISTING AS RECORDS (engine_listing_record.rs, engine_heads.rs `Listings`, D149
//! piece I2), at the engine's seam. A child of main.rs's tests, over engine_check_tests.rs's
//! helpers.
//!
//! THE FLOOR: after an edit that moves none of a listing's inputs (one fn's body), the crate's
//! listing is one read of its join record: 0 made, no file's part made, no walk. After an item
//! is added to one file, that file's part alone is made, the other's read, and the crate joined
//! again. A restored listing equals a fresh build's under the same key (rule 26).
//! CONTROL (rule 23): `ListingsPlant::PerAsk` takes the table and the store away; the same body
//! edit then reads nothing and makes the listing again.
//!
//! UNITS: the counters count a crate as its UNIT views it, and a member package is held by more
//! than one unit (observed 2026-11-19: its library unit and its test unit, so 2 crates and 4
//! file parts for 1 package of 2 files; engine_owed_listing_tests.rs reads its counts the same
//! way). Every expectation here is per unit: `units` is the cold run's crates.

use super::engine_check_tests::{engine_opts, opts};
use super::*;

use crate::engine_heads::{HeadJobs, ListingCounts, ListingsPlant};

const WORKSPACE: &str = "[workspace]\nmembers = [\"one\"]\nresolver = \"2\"\n";
const MANIFEST: &str = "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
/// The crate root: a struct and a fn over it, and the second file's module.
const LIB: &str = "pub mod a;\n\npub struct P {\n    pub x: u32,\n}\n\npub fn make(x: u32) -> P {\n    P { x }\n}\n";
/// The second file: fns over the root's struct, one calling the other.
const A: &str = "use crate::P;\n\npub fn get(p: &P) -> u32 {\n    p.x\n}\n\npub fn twice(p: &P) -> u32 {\n    get(p) + get(p)\n}\n";
/// `A` with one body edited (`twice`'s): no listing input moves.
const A_BODY: &str = "use crate::P;\n\npub fn get(p: &P) -> u32 {\n    p.x\n}\n\npub fn twice(p: &P) -> u32 {\n    get(p) * 2\n}\n";
/// `A` with one item added at its end: its file's entries and the items record move.
const A_ITEM: &str = "use crate::P;\n\npub fn get(p: &P) -> u32 {\n    p.x\n}\n\npub fn twice(p: &P) -> u32 {\n    get(p) + get(p)\n}\n\npub fn thrice(p: &P) -> u32 {\n    get(p) + twice(p)\n}\n";
/// The package's files.
const FILES: u32 = 2;

/// A two-file crate whose second file is `a`.
fn two_files(seed: u64, a: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "one/Cargo.toml", MANIFEST);
    write(&io, "one/src/lib.rs", LIB);
    write(&io, "one/src/a.rs", a);
    io
}

/// One request on one worker (no race) over `io`'s store, its head jobs as `jobs` say: the
/// listings' counts.
fn request(io: &Io, jobs: HeadJobs) -> ListingCounts {
    let mut o = opts(io, 1, false);
    o.head_jobs = jobs;
    let (_, checked) = engine_opts(io, o);
    checked.seeds.units.listings
}

/// The cold run of a fresh home: each unit's crate listed once, each of its files' parts made.
/// Its units.
fn cold_units(cold: &ListingCounts) -> u32 {
    let units = cold.crates;
    assert!(units >= 1, "the cold request listed the crate in at least one unit (the check ran): {cold:?}");
    assert_eq!(cold.read, 0, "a cold store holds no listing: {cold:?}");
    assert_eq!(cold.made, units, "cold: each unit's listing made once: {cold:?}");
    assert_eq!(cold.parts_made, FILES * units, "cold: each of the {FILES} files' parts made, once a unit: {cold:?}");
    assert_eq!(cold.parts_read, 0, "cold: no part read: {cold:?}");
    assert_eq!(cold.damaged + cold.unstored + cold.faulted + cold.unequal + cold.full, 0, "cold: nothing refused, damaged or unstored: {cold:?}");
    units
}

/// The first listing of `restored` that `fresh` holds under another digest, or does not hold:
/// (crate, files, restored digest, fresh digest). Rule 26 at the listing's grain: every listing
/// a restore read equals the fresh build's under its key (the digest is its rows', checked
/// against them at its open).
fn first_unequal(restored: &ListingCounts, fresh: &ListingCounts) -> Option<std::string::String> {
    restored.each.shown().iter().find_map(|(krate, files, digest)| match fresh.each.at(*krate, *files) {
        Some(other) if other == *digest => None,
        other => Some(format!("crate {krate:?} files {files:?}: restored rows {digest:?}, fresh {other:?}")),
    })
}

/// Expected lines: `cold: made U, ... parts 2U made`, then `body edit: made 0, read R ...`.
#[test]
fn a_one_body_edit_reads_the_listing_and_makes_none() {
    let io = two_files(41, A);
    let cold = request(&io, HeadJobs::RULE);
    println!("cold: made {}, read {}, parts {} made {} read, {} crates, {} asks", cold.made, cold.read, cold.parts_made, cold.parts_read, cold.crates, cold.asks);
    let units = cold_units(&cold);

    write(&io, "one/src/a.rs", A_BODY);
    let edit = request(&io, HeadJobs::RULE);
    println!("body edit: made {}, read {}, parts {} made {} read, {} crates, {} asks", edit.made, edit.read, edit.parts_made, edit.parts_read, edit.crates, edit.asks);
    assert!(edit.asks > 0 && edit.crates >= 1, "the edit's request asked a listing (a body's reads were placed): {edit:?}");
    assert!(edit.crates <= units, "the edit asks no unit the cold run did not list: {edit:?} of {units} units");
    assert_eq!(edit.made, 0, "THE FLOOR: a one-body edit makes no listing in any unit: {edit:?}");
    assert_eq!(edit.read, edit.crates, "THE FLOOR: each unit's listing asked is one read of its join: {edit:?}");
    assert_eq!(edit.parts_made + edit.parts_read, 0, "a read join opens no file's part: {edit:?}");
    assert_eq!(edit.damaged, 0, "the stored joins opened: {edit:?}");

    // CONTROL: the plant that takes the table and the store away reads nothing on the same edit.
    let io = two_files(41, A);
    let _ = request(&io, HeadJobs { listings: ListingsPlant::PerAsk, ..HeadJobs::RULE });
    write(&io, "one/src/a.rs", A_BODY);
    let planted = request(&io, HeadJobs { listings: ListingsPlant::PerAsk, ..HeadJobs::RULE });
    println!("control: made {}, read {}, {} asks", planted.made, planted.read, planted.asks);
    assert_eq!(planted.read, 0, "CONTROL: no store, no read: {planted:?}");
    assert!(planted.made > 0, "CONTROL: the listing is made again: {planted:?}");
}

/// Expected line: `item added: made J, parts J made J read` with J the units asked.
#[test]
fn an_item_added_to_one_file_makes_that_files_part_alone() {
    let io = two_files(43, A);
    let units = cold_units(&request(&io, HeadJobs::RULE));
    write(&io, "one/src/a.rs", A_ITEM);
    let added = request(&io, HeadJobs::RULE);
    println!("item added: made {}, read {}, parts {} made {} read, {} crates", added.made, added.read, added.parts_made, added.parts_read, added.crates);
    assert!(added.crates >= 1 && added.crates <= units, "the edit asked a listing in at most the cold run's units: {added:?} of {units}");
    assert_eq!(added.read, 0, "the join's key moved with the file's entries, in every unit: {added:?}");
    assert_eq!(added.made, added.crates, "each unit's crate is joined again: {added:?}");
    assert_eq!(added.parts_made, added.made, "the edited file's part alone is made, once a unit: {added:?}");
    assert_eq!(added.parts_read, added.made * (FILES - 1), "the other file's part is read by its key, once a unit: {added:?}");
    assert_eq!(added.damaged + added.unstored + added.faulted, 0, "nothing damaged or unstored: {added:?}");

    // CONTROL: the same tree in a fresh home makes every part, so the reads above were the
    // store's and not the request's own.
    let fresh = request(&two_files(43, A_ITEM), HeadJobs::RULE);
    assert_eq!((fresh.parts_made, fresh.parts_read), (FILES * fresh.made, 0), "CONTROL: a fresh home makes each part, once a unit: {fresh:?}");
}

/// Expected line: `restore: R listings read, each equal to the fresh build's (of F)`.
#[test]
fn a_restored_listing_equals_a_fresh_one_in_rows_and_bytes() {
    // Restored: made cold, then read after a body edit.
    let io = two_files(47, A);
    let _ = request(&io, HeadJobs::RULE);
    write(&io, "one/src/a.rs", A_BODY);
    let restored = request(&io, HeadJobs::RULE);
    // Fresh: the edited tree in a home of its own.
    let fresh = request(&two_files(47, A_BODY), HeadJobs::RULE);
    // Both paths ran (rule 61): one read, one made.
    assert!(restored.read > 0 && restored.made == 0, "the restore read its listings: {restored:?}");
    assert!(fresh.made > 0 && fresh.read == 0, "the fresh build made its listings: {fresh:?}");
    assert_eq!((restored.each.over, fresh.each.over), (0, 0), "every listing shown: {restored:?} {fresh:?}");
    assert_eq!(u32::from(restored.each.len), restored.crates, "each listing read is shown: {restored:?}");
    println!("restore: {} listings read, fresh: {} made", restored.each.len, fresh.each.len);
    // Key by key: the request-wide digests (`rows`, `bytes`) cover the set each request asked,
    // and an edit's request asks only the units it places reads in.
    assert_eq!(first_unequal(&restored, &fresh), None, "a restored listing is the fresh build's under its key (rule 26)");
    // Asked the same set, the request-wide digests agree too.
    if restored.crates == fresh.crates {
        assert_eq!((restored.rows, restored.bytes), (fresh.rows, fresh.bytes), "the same listings asked: the same rows and join bytes");
    }

    // CONTROL: a tree whose listing differs (an item added) differs under the same key, so the
    // comparison above sees each listing's rows.
    let other = request(&two_files(47, A_ITEM), HeadJobs::RULE);
    let unequal = first_unequal(&other, &fresh);
    println!("control: {unequal:?}");
    assert!(unequal.is_some(), "CONTROL: another listing under the same key is found unequal: {other:?} {fresh:?}");
}
