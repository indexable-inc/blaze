//! The store load hands an expansion's origins as the TWO parts they are stored in (front_load.rs
//! `StoreLoad::expansion`'s `LoadedExpansion::origins`, lane M14): the crate directory's call part and the expansion
//! record's column, read where they lie, never joined into a new block.
//!
//! Floor (rule 67): 0 allocations and 0 bytes in the load's allocator for any number of asks.
//! The cause removed (rule 23): before M14 the first ask of each call copied its call part and
//! its column into one new block of the load's allocator, kept to the command's end
//! (`ExpansionTexts::joined_origins` and the `StoreLoad::joined` table: 295 MB on tempo's names
//! stage, the 2026-11-18 heap log), so this count was one block per call with a column plus the
//! table's growths. CONTROL: a block made in the counter is counted. ORACLE: the two parts are,
//! in order, exactly the bytes of the solve's joined origins, and `OriginsView::open_parts` opens
//! them as `OriginsView::open` opens the solve's bytes.
//!
//! Expected (`cargo test -p blaze-cli --bin blaze -- origins_parts_tests --nocapture`):
//!   `origins in parts: C calls asked twice, W with a column, 0 allocations (floor 0), 0 bytes
//!   live (floor 0)` and ok.

use blaze_ladder::front2::source::Load;
use blaze_own::testing::Counting;
use blaze_own::{AVec, Erased};
use blaze_scope::expansion_origins::OriginsView;
use bstd::mem::{Frame, Tag};

use super::tests::{Files, kept, payloads, scope_name, solved};
use super::{CrateRecords, StoreLoad, crate_records};

#[test]
fn a_store_loads_origins_are_two_parts_and_no_copy() {
    let names = solved();
    // The test's memory: one frame, handed down; the load's allocator counts over it.
    let frame = Frame::new(Tag::Other).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let home = Erased::new(&frame);
    let bump = home.bump();
    let alloc = bump.alloc();
    let payloads = payloads(&names);
    let snapshot = kept(&names, true);
    let mut read: AVec<'_, CrateRecords<'_>> = AVec::with_capacity_in(names.crates().len(), alloc);
    for (c, payload) in names.crates().iter().zip(payloads.iter()) {
        read.push(crate_records(scope_name(c.key), payload, c.key, bump).unwrap_or_else(|e| panic!("crate '{}': {e}", c.name)));
    }
    let mut packages: AVec<'_, &str> = AVec::with_capacity_in(read.len(), alloc);
    packages.resize(read.len(), "/");
    let counting = Erased::new(Counting::new(&frame));
    let load = StoreLoad::from_parts(&read[..], &packages[..], &Files, Some(&snapshot), None, None, counting.handle());

    // Control: a block made in the counter is counted.
    let before = counting.inner().allocations();
    let probe: AVec<'_, u8> = AVec::with_capacity_in(1, counting.handle());
    drop(probe);
    assert_eq!(counting.inner().allocations(), before + 1, "control: the counter does not count");

    let (before, bytes_before) = (counting.inner().allocations(), counting.inner().live_bytes());
    let mut calls = 0usize;
    let mut with_column = 0usize;
    // Twice, as the ladder asks again at each mark it reads (front2 source.rs `text_marks`).
    for round in 0..2 {
        for (krate, c) in names.crates().iter().enumerate() {
            for (call, expansion) in c.expansions.iter() {
                let Some(made) = expansion.made else { continue };
                // One read serves the text and its origins (F4).
                let read = load.expansion(krate, *call).unwrap_or_else(|| panic!("crate '{}' call {call:?}: no expansion served", c.name));
                assert_eq!(read.text, made.text, "crate '{}' call {call:?}: its text over the store", c.name);
                let (call_part, column) = read.origins.unwrap_or_else(|| panic!("crate '{}' call {call:?}: no origins served", c.name));
                if round == 0 {
                    calls += 1;
                }
                match made.origins {
                    Ok(bytes) if !bytes.is_empty() => {
                        // The oracle: the parts are the joined bytes, cut where the column starts.
                        assert_eq!(call_part, &bytes[..call_part.len()], "crate '{}' call {call:?}: the call part", c.name);
                        assert_eq!(column, &bytes[call_part.len()..], "crate '{}' call {call:?}: the column", c.name);
                        let parts = OriginsView::open_parts(call_part, column).unwrap_or_else(|e| panic!("crate '{}' call {call:?}: the parts open: {e}", c.name));
                        let whole = OriginsView::open(bytes).unwrap_or_else(|e| panic!("crate '{}' call {call:?}: the solve's bytes open: {e}", c.name));
                        assert_eq!((parts.text, parts.item, parts.input_start, parts.input_len, parts.macro_name), (whole.text, whole.item, whole.input_start, whole.input_len, whole.macro_name));
                        if round == 0 {
                            with_column += 1;
                        }
                    }
                    // A text that keeps none: an empty call part (the trait's docs).
                    _ => assert!(call_part.is_empty(), "crate '{}' call {call:?}: a text with no origins handed a call part", c.name),
                }
            }
        }
    }
    let made = counting.inner().allocations() - before;
    let bytes = counting.inner().live_bytes() - bytes_before;
    println!("origins in parts: {calls} calls asked twice, {with_column} with a column, {made} allocations (floor 0), {bytes} bytes live (floor 0)");
    assert!(with_column >= 1, "the fixture's macro call (`lib`'s `write_two!`) keeps a column: {calls} calls, none with one");
    assert_eq!(made, 0, "the load made a block for an origins ask: a joined copy");
    assert_eq!(bytes, 0);
    assert!(load.unserved().is_empty(), "a call was not served: {:?}", &*load.unserved());
}
