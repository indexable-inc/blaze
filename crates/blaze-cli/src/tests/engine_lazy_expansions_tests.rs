//! Lazy expansion-text reads for Bodies. The Bodies stage opens an expansion record only when a
//! body it lists or checks is itself written in that expansion: its text is the expansion's
//! (front2 project_records.rs `list_with` reads `text_of` of each fn, const and static; front2
//! source/item_grain.rs `row_now` and `span_now` read the expansion's whole text through
//! blaze-scope items_record.rs `ItemsRecord::whole`). A body that only NAMES a macro-written item
//! takes that item's head from the request's frozen heads (engine_bodies.rs `run_stretch`,
//! `BodySeed`), which the Heads stage made under its own snapshot, so the Bodies stage demands no
//! text for it. Within one opened `Records` the whole text is fetched once (`ItemsRecord::whole`
//! keeps it in its `seen` memo), so the stage's demands are one per opening of the member's
//! records that reads the text, never one per body.

use super::*;

fn library(io: &Io, source: &str) {
    write(io, "Cargo.toml", "[package]\nname = \"lazy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\ntest = false\n");
    write(io, "src/lib.rs", source);
}

fn body_reads(io: &Io) -> std::vec::Vec<std::vec::Vec<(blaze_engine::CellId, blaze_ids::ContentId)>> {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let frame = cx.frame();
        let root = crate::engine_answer::root_name(opts.place.checkout, frame);
        let pointer = crate::engine_answer::load_pointer(cx, store, &root)
            .expect("the stored answer reads")
            .expect("the lazy expansion check stores an answer")
            .expect("the stored pointer is readable");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared())
            .expect("the answer pages read")
            .expect("the answer rows open");
        kept.answer
            .rows
            .iter()
            .filter(|(name, _)| name.kind == blaze_drive::Kind::BodyCheck)
            .map(|(_, row)| row.reads.iter().copied().collect())
            .collect()
    })
}

fn cells_options(io: &Io, fresh: bool) -> crate::engine_check::Opts<'static> {
    let mut opts = super::engine_check_tests::opts(io, 2, fresh);
    opts.scope_reads = crate::engine_scope_reads::ScopeReadsAs::Cells;
    opts
}

/// The units that hold the members, from the Bodies profile's owed-listing line ("bodies:
/// members opened M of T, ...": T counts UNITS, engine_owed_listing_tests.rs `counters`).
fn units_of(io: &Io) -> u64 {
    let profile = super::engine_check_tests::profile(io);
    let line = profile
        .lines()
        .find_map(|line| line.strip_prefix("bodies: members opened "))
        .unwrap_or_else(|| panic!("the Bodies profile says which members it opened:\n{profile}"));
    line.split_whitespace()
        .nth(2)
        .and_then(|word| word.trim_end_matches([',', ';']).parse().ok())
        .unwrap_or_else(|| panic!("word 2 of the members line is the unit count: {line}"))
}

fn expansion_reads(io: &Io) -> (u64, u64) {
    super::engine_check_tests::profile(io)
        .lines()
        .find_map(|line| line.strip_prefix("bodies: expansion text record demands "))
        .and_then(|counts| {
            let mut words = counts.split_whitespace();
            let demands = words.next()?.trim_end_matches(',').parse().ok()?;
            if words.next()? != "distinct" || words.next()? != "keys" {
                return None;
            }
            Some((demands, words.next()?.trim_end_matches(',').parse().ok()?))
        })
        .expect("the Bodies profile reports expansion-text demands and distinct keys")
}

/// The macro call writes a struct and no body; the edit changes only `plain`'s body. Checking
/// `plain` (its head `fn plain() -> u8` and its body `2`) depends on no token the expansion
/// wrote: by rule 77 (work-once: never compute what a key can skip) the Bodies stage demands no
/// expansion text, so the floor is (0 demands, 0 distinct keys). The stage's listing reads the
/// text only of a fn, const or static a macro wrote (front2 project_records.rs `list_with`), and
/// `plain`'s task reads its own file entry; a demand here is a read to remove at its cause, not
/// a floor to assert.
#[test]
fn editing_a_plain_body_beside_a_struct_writing_call_demands_no_expansion_text() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    library(
        &io,
        "macro_rules! make_type { () => { pub struct Generated; }; }\nmake_type!();\n\npub fn plain() -> u8 {\n    1\n}\n",
    );
    let _ = super::engine_check_tests::engine_opts(&io, cells_options(&io, true));

    write(&io, "src/lib.rs", "macro_rules! make_type { () => { pub struct Generated; }; }\nmake_type!();\n\npub fn plain() -> u8 {\n    2\n}\n");
    let _ = super::engine_check_tests::engine_opts(&io, cells_options(&io, false));
    assert_eq!(expansion_reads(&io), (0, 0), "editing only `plain` demands no expansion text: no body the edit re-checks is written by `make_type!`");
    std::println!("plain body edit: expansion text demands 0; distinct keys 0");
}

/// Two bodies that NAME a macro-written struct, checked fresh: each takes `Generated`'s head from
/// the request's frozen heads (engine_bodies.rs `run_stretch`, `BodySeed`; made by the Heads
/// stage under its own snapshot), and neither body's text is the expansion's, so the Bodies
/// stage demands no expansion text: (0, 0).
#[test]
fn bodies_that_name_a_macro_written_struct_demand_no_expansion_text() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    library(
        &io,
        "macro_rules! make_type { () => { pub struct Generated; }; }\nmake_type!();\n\npub fn first() -> Generated {\n    Generated\n}\n\npub fn second() -> Generated {\n    Generated\n}\n",
    );
    let _ = super::engine_check_tests::engine_opts(&io, cells_options(&io, true));
    assert_eq!(expansion_reads(&io), (0, 0), "a body that names a macro-written item reads its frozen head, never the expansion's text");
    std::println!("bodies naming a macro-written struct: expansion text demands 0; distinct keys 0");
}

/// Two bodies ONE macro call wrote, checked fresh. Both are checked by the one task of the call's
/// entry (engine_bodies.rs `groups`, step 2: each body an expansion at the entry wrote is in that
/// entry's list), so the answer holds one `BodyCheck` row for them. The expansion's text is the
/// text of both bodies; within one opened `Records` it is fetched once and kept
/// (blaze-scope items_record.rs `ItemsRecord::whole`, its `seen` memo). The stage opens the
/// member's records twice and each opening reads it: `groups` lists the two fns (front2
/// project_records.rs `list_with` reads `text_of` of each), and the entry's stretch checks them
/// (engine_bodies.rs `run_stretch`). So, a unit that holds the package, 1 listing demand of its
/// own expansion record (the unit's crate key is in the record's column), and 1 stretch demand
/// for the unit that checks: one per opening, never one per body (observed (3, 2) with two
/// units, 2026-11-19, when the first holder alone checked; since D191 every holder checks, a
/// row each, so (4, 2) with two units). The row keeps a read of the expansion's cell, so an edit of the macro's text
/// re-checks both bodies.
#[test]
fn two_bodies_one_call_wrote_demand_its_text_once_per_records_opening() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    library(
        &io,
        "macro_rules! two {\n    () => {\n        pub fn first() -> u8 { 1 }\n        pub fn second() -> u8 { 2 }\n    };\n}\ntwo!();\n",
    );
    let _ = super::engine_check_tests::engine_opts(&io, cells_options(&io, true));
    // Per unit (as engine_owed_listing_tests.rs reads its counters): the one package is held by
    // `units` units, and each unit's records hold their OWN expansion record of `two!()`: its
    // column's marks carry the def site's crate (blaze-expand origin_column.rs `TextMark::site`,
    // blaze-surface macro_def.rs `DefSite::krate`), each unit's crate key, and the record's key
    // is the digest of text and column (blaze-scope expansion_records.rs `key_of`). `groups`
    // opens each unit's records and lists it (engine_bodies.rs `groups`, the
    // `names_records_with_snapshot` there): 1 demand a unit. The two bodies are checked under
    // the first unit alone (engine_bodies.rs `Decision::Taken`, "checked under the first"), so
    // the entry's stretch adds 1 demand of that unit's key. Floor: units + 1 demands of `units`
    // distinct keys; one unit gives (2, 1). When the second unit checks too, its stretch adds 1.
    // D191: every unit that holds a body checks it (no longer "under the first unit alone"), so
    // each unit's stretch adds its 1 demand: 2 demands a unit, of `units` distinct keys.
    let units = units_of(&io);
    assert!(units >= 1, "the package is held by at least one unit");
    assert_eq!(expansion_reads(&io), (2 * units, units), "each unit's listing fetches its own expansion record of the call once, and each unit's stretch, every holder checking (D191), fetches it once more ({units} unit(s))");

    let rows = body_reads(&io);
    assert_eq!(rows.len(), units as usize, "the call's entry is one task checking both written bodies, a row a holding unit (D191): {rows:?}");
    let row = &rows[0];
    let cell_reads: std::vec::Vec<_> = row
        .iter()
        .filter(|(id, _)| id.of.kind == blaze_drive::Kind::CrateScope && id.cell != crate::engine_tasks::WHOLE && id.cell != crate::engine_tasks::CELLS)
        .collect();
    assert!(!cell_reads.is_empty(), "the row of the bodies `two!` wrote retains a read of the expansion's cell: {row:?}");
    std::println!("one call's two bodies: 2 demands of 1 distinct key; their row reads the expansion cell");
}
