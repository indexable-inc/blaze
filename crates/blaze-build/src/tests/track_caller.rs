//! `#[track_caller]` (walk/track_caller.rs): the hidden last parameter, made at an untracked
//! caller and forwarded by a tracked one.
//!
//! The world (keys in brackets):
//! ```text
//! struct Location { filename: &str, line: u32, col: u32 }   // [20], `Adts::location_adt`
//! fn main(x: u32) -> u32 { callee(x) }                       // [1], tracked or not by case
//! #[track_caller] fn callee(x: u32) -> u32;                  // [3]
//! ```
//! The Reference ("The `track_caller` attribute"): the attribute "allows code within the
//! function to get a hint of the Location of the top-most tracked call that led to the
//! function's invocation". So an untracked `main`'s call of `callee` passes a `Location` of the
//! call's place, and a tracked `main` passes its own, which it took as its last parameter.

use super::*;

const LOCATION: ItemKey = ItemKey(20);
const CALLEE: ItemKey = ItemKey(3);

/// `main` (tracked when `tracked_main`) calling `callee(x)`, built.
fn build_main(tracked_main: bool) -> Result<Built<Global>, String> {
    let mut w = TestWorld::new();
    let str_ref = w.ref_(Mutbl::Not, PNode::Str);
    w.strukt(20, "Location", &[("filename", str_ref), ("line", U32), ("col", U32)]);
    w.location = Some(LOCATION);
    w.func(1, "main", &[U32], U32);
    w.func(3, "callee", &[U32], U32);
    w.tracked.push(CALLEE);
    if tracked_main {
        w.tracked.push(MAIN);
    }
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let x_e = b.local(x);
    let root = b.call("callee", &[x_e]);
    build(&w, &b, root)
}

/// An untracked `main`: its call of `callee` passes a `Location` row of [`TRACK_SITE`]
/// (item-relative: file, item, offset), whose line and column are two source positions of
/// that site, their bytes zero (never an absolute line). A tracked `main`: it takes the pointer
/// as its own last parameter and passes it on with no row of its own.
///
/// A builder that writes the row with a line fails the zero-bytes `assert`; one without the
/// hidden parameter fails a call-signature `assert` (the call is `(i32) -> i32`); one that
/// makes a row in a tracked fn instead of forwarding fails the last.
/// Prints `track caller: an untracked caller passes a row of two source positions, a tracked one forwards its own`.
#[test]
fn a_tracked_call_passes_its_location_last() {
    let main = build_main(false).unwrap_or_else(|e| panic!("{e}"));
    let view = main.func.view();
    let text = blaze_lir::print(view);
    assert!(has_call_sig(&main, &[Ty::I32, Ty::PTR], &[Ty::I32]), "{text}");
    let with = (0..view.datas.len() as u32).map(blaze_lir::func::DataId).find(|&d| !view.data_positions_of(d).is_empty());
    let row = with.unwrap_or_else(|| panic!("no `Location` row with source positions: {text}"));
    let positions = view.data_positions_of(row);
    assert_eq!(positions.len(), 2, "a line and a column: {text}");
    for p in positions {
        assert_eq!((p.item_key(), p.site), (TRACK_SITE.item.0, TRACK_SITE.offset), "{text}");
        let bytes = view.data_bytes_of(row);
        assert_eq!(&bytes[p.offset as usize..p.offset as usize + 4], &[0; 4], "no absolute line or column: {text}");
    }

    let main = build_main(true).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(main.func.view());
    assert_eq!(own_sig(&main), (vec_of(&[Ty::I32, Ty::PTR]), vec_of(&[Ty::I32])), "{text}");
    assert!(has_call_sig(&main, &[Ty::I32, Ty::PTR], &[Ty::I32]), "{text}");
    assert!(main.func.view().datas.is_empty(), "a tracked fn forwards, it makes no row: {text}");
    println!("track caller: an untracked caller passes a row of two source positions, a tracked one forwards its own");
}
