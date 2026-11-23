//! The checked-body record (src/checked_record.rs, D163 R1) round-trips (rule 26).
//!
//! Every clean body the suite checks goes through [`round_trips`] from `check_body`, the
//! suite's one entry: its record decodes, the decoded record re-encodes to equal bytes, its
//! columns equal the check's, and the record of the check's second run (the stop strategy)
//! has equal bytes (two checks of one body encode equal). The tests here are the controls:
//! each removes what the property depends on and watches it fail.

use std::format;
use std::string::String;

use blaze_own::Alloc;

use super::prim_case;
use crate::checked_record::{COLS, CheckedRecord, CheckedRecordError, Col, decode, encode};
use crate::columns::Checked;
use crate::ids::{ExprId, LangItem};
use crate::portable::PTy;

/// The columns the record holds, equal between `a` and `b` (what `same_result` compares, less
/// the columns the record leaves out); the first column that differs, by name.
fn differs(a: &Checked<'_>, b: &Checked<'_>) -> Option<&'static str> {
    let same = [
        ("types", a.types == b.types),
        ("expr_ty", a.expr_ty == b.expr_ty),
        ("pat_ty", a.pat_ty == b.pat_ty),
        ("local_ty", a.local_ty == b.local_ty),
        ("adjusts", a.adjusts.column::<0>() == b.adjusts.column::<0>() && a.adjusts.column::<1>() == b.adjusts.column::<1>() && a.adjusts.column::<2>() == b.adjusts.column::<2>()),
        ("callees", a.callees.column::<0>() == b.callees.column::<0>() && a.callees.column::<1>() == b.callees.column::<1>() && a.callees.column::<2>() == b.callees.column::<2>()),
        ("expr_defs", a.expr_defs.column::<0>() == b.expr_defs.column::<0>() && a.expr_defs.column::<1>() == b.expr_defs.column::<1>()),
        ("const_args", a.const_args.column::<0>() == b.const_args.column::<0>() && a.const_args.column::<1>() == b.const_args.column::<1>()),
        ("pat_const_args", a.pat_const_args.column::<0>() == b.pat_const_args.column::<0>() && a.pat_const_args.column::<1>() == b.pat_const_args.column::<1>()),
        ("pat_defs", a.pat_defs.column::<0>() == b.pat_defs.column::<0>() && a.pat_defs.column::<1>() == b.pat_defs.column::<1>()),
        ("union_pats", a.union_pats.column::<0>() == b.union_pats.column::<0>() && a.union_pats.column::<1>() == b.union_pats.column::<1>()),
        ("fields", a.fields.column::<0>() == b.fields.column::<0>() && a.fields.column::<1>() == b.fields.column::<1>() && a.fields.column::<2>() == b.fields.column::<2>()),
        ("captures", a.captures.column::<0>() == b.captures.column::<0>() && a.captures.column::<1>() == b.captures.column::<1>() && a.captures.column::<2>() == b.captures.column::<2>()),
        (
            "capture_places",
            a.capture_places.column::<0>() == b.capture_places.column::<0>()
                && a.capture_places.column::<1>() == b.capture_places.column::<1>()
                && a.capture_places.column::<2>() == b.capture_places.column::<2>()
                && a.capture_places.column::<3>() == b.capture_places.column::<3>()
                && a.capture_places.column::<4>() == b.capture_places.column::<4>()
                && a.capture_places.column::<5>() == b.capture_places.column::<5>(),
        ),
        (
            "capture_projs",
            a.capture_projs.column::<0>() == b.capture_projs.column::<0>()
                && a.capture_projs.column::<1>() == b.capture_projs.column::<1>()
                && a.capture_projs.column::<2>() == b.capture_projs.column::<2>()
                && a.capture_projs.column::<3>() == b.capture_projs.column::<3>(),
        ),
        (
            "closures",
            a.closures.column::<0>() == b.closures.column::<0>()
                && a.closures.column::<1>() == b.closures.column::<1>()
                && a.closures.column::<2>() == b.closures.column::<2>()
                && a.closures.column::<3>() == b.closures.column::<3>()
                && a.closures.column::<4>() == b.closures.column::<4>(),
        ),
        ("hidden", a.hidden == b.hidden),
        ("promoted", a.promoted == b.promoted),
        ("coroutine_witnesses", a.coroutine_witnesses == b.coroutine_witnesses),
        ("awaited", a.awaited == b.awaited),
        ("selected", a.selected == b.selected),
        ("fru_fields", a.fru_fields.column::<0>() == b.fru_fields.column::<0>() && a.fru_fields.column::<1>() == b.fru_fields.column::<1>() && a.fru_fields.column::<2>() == b.fru_fields.column::<2>()),
        (
            "default_fields",
            a.default_fields.column::<0>() == b.default_fields.column::<0>()
                && a.default_fields.column::<1>() == b.default_fields.column::<1>()
                && a.default_fields.column::<2>() == b.default_fields.column::<2>(),
        ),
        ("index_mut", a.index_mut == b.index_mut),
        ("needs", a.needs == b.needs),
    ];
    same.iter().find(|(_, s)| !s).map(|(n, _)| *n)
}

/// The property, for one body checked twice (`one` by the pull, `two` by the stop): a clean
/// check's record decodes to the check's columns, re-encodes to equal bytes, and the second
/// check's record is the same bytes. A check that is not clean is refused by name, and kept
/// no record of.
pub(super) fn round_trips(body: &str, one: &Checked<'_>, two: &Checked<'_>) {
    let alloc = Alloc::global();
    let bytes = match encode(one, alloc) {
        Ok(b) => b,
        Err(CheckedRecordError::NotClean { .. }) if !one.diags.is_empty() || !one.stalled.is_empty() || !one.undecided.is_empty() => return,
        Err(e) => panic!("`{body}`: the record of a clean check was refused: {e}"),
    };
    let decoded = decode(&bytes, alloc).unwrap_or_else(|e| panic!("`{body}`: its own record does not decode: {e}"));
    if let Some(col) = differs(one, &decoded) {
        panic!("`{body}`: the decoded record's `{col}` is not the check's:\n{one:?}\n{decoded:?}");
    }
    let again = encode(&decoded, alloc).unwrap_or_else(|e| panic!("`{body}`: the decoded record does not encode: {e}"));
    assert!(bytes[..] == again[..], "`{body}`: the decoded record re-encodes to other bytes ({} and {})", bytes.len(), again.len());
    let second = encode(two, alloc).unwrap_or_else(|e| panic!("`{body}`: the second check's record was refused: {e}"));
    assert!(bytes[..] == second[..], "`{body}`: two checks of one body encode to other bytes ({} and {})", bytes.len(), second.len());
}

/// `fn c_projection(a: A) -> u32 { u32::pow(2, 3) }`: a body with a callee, types and lists.
fn a_call() -> Checked<'static> {
    let (_fx, _b, c) = prim_case(|b| {
        let f = b.path(&["u32", "pow"]);
        let x = b.int(None);
        let y = b.int(None);
        b.call(f, &[x, y])
    });
    c
}

fn bytes_of(c: &Checked<'_>) -> blaze_own::Vec<u8, Alloc<'static>> {
    encode(c, Alloc::global()).unwrap_or_else(|e| panic!("the record: {e}"))
}

/// Control of the column comparison and the re-encode: a decoded record with one expression's
/// type changed differs from the check by name, and re-encodes to other bytes. Without the
/// change both are equal (the property holds on this body).
#[test]
fn control_a_changed_column_is_seen_by_the_comparison_and_the_bytes() {
    let c = a_call();
    assert!(c.diags.is_empty(), "the body is clean: {:?}", c.diags);
    let bytes = bytes_of(&c);
    let mut d = decode(&bytes, Alloc::global()).unwrap_or_else(|e| panic!("decode: {e}"));
    assert_eq!(differs(&c, &d), None, "control's baseline: the decoded record is the check's");
    assert!(bytes[..] == bytes_of(&d)[..], "control's baseline: the decoded record re-encodes equal");
    assert!(c.callees.len() > 0, "the body has a callee row, so the callees column is written");
    let other = if d.expr_ty[0] == PTy(0) { PTy(1) } else { PTy(0) };
    d.expr_ty[0] = other;
    assert_eq!(differs(&c, &d), Some("expr_ty"), "the planted change is seen by name");
    assert!(bytes[..] != bytes_of(&d)[..], "the planted change is seen in the bytes");
}

/// Control of "two checks encode equal": two different bodies do not.
#[test]
fn control_two_bodies_encode_to_other_bytes() {
    let one = bytes_of(&a_call());
    let (_fx, _b, max) = prim_case(|b| b.path(&["u32", "MAX"]));
    assert!(one[..] != bytes_of(&max)[..], "two bodies' records are equal: the record holds nothing of the body");
}

/// The record refuses what it is not: a check with a diagnostic, bytes of another version, a
/// cut record, and a tag no type has (each by name, rule 19).
#[test]
fn the_record_refuses_by_name() {
    // `u32::MIN`: no item has it in this world (`unknown_item_on_a_primitive_type_names_both`).
    let (_fx, _b, bad) = prim_case(|b| b.path(&["u32", "MIN"]));
    assert!(!bad.diags.is_empty(), "control: the body has a diagnostic");
    match encode(&bad, Alloc::global()) {
        Err(CheckedRecordError::NotClean { what: "diags", .. }) => {}
        other => panic!("a check with a diagnostic was not refused: {:?}", other.map(|b| b.len())),
    }
    let bytes = bytes_of(&a_call());
    let mut other = bytes[..].to_vec();
    other[7] = b'9';
    assert_eq!(decode(&other, Alloc::global()).map(drop), Err(CheckedRecordError::Magic));
    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(decode(cut, Alloc::global()).map(drop), Err(CheckedRecordError::Length { .. })), "a cut record decoded");
    let rec = CheckedRecord::open(&bytes).unwrap_or_else(|e| panic!("open: {e}"));
    assert!(rec.rows(Col::TyNodes) > 0, "the body has types");
    let header = 8 + 4 + 4 * COLS.len();
    let mut tagged = bytes[..].to_vec();
    tagged[header] = 0xee;
    match decode(&tagged, Alloc::global()) {
        Err(CheckedRecordError::Tag { column: "types.nodes", row: 0, tag: 0xee }) => {}
        other => panic!("a node with tag 0xee decoded: {:?}", other.map(|_| ())),
    }
}

/// The captured places (stage 2, `bzchkd04`) round-trip: a place row and its projection run
/// decode to the rows written and re-encode equal. Twins: a projection kind no projection has
/// (3) and a place whose run passes the projection column are refused by name.
#[test]
fn captured_places_round_trip_and_refuse_by_name() {
    use crate::columns::CaptureKind;
    use crate::ids::PatId;
    let bytes = bytes_of(&a_call());
    let mut d = decode(&bytes, Alloc::global()).unwrap_or_else(|e| panic!("decode: {e}"));
    assert!(d.capture_places.len() == 0 && d.capture_projs.len() == 0, "control: a body with no closure has no place rows");
    // `|| x.1` by value, `x: (A, B)`: one place, one field projection.
    d.capture_projs.push((1, 0, 1, PTy(0)));
    d.capture_places.push((ExprId(0), PatId(0), 0, 1, CaptureKind::ByValue, PTy(0)));
    let with = bytes_of(&d);
    assert!(with[..] != bytes[..], "the place rows are in the bytes");
    let back = decode(&with, Alloc::global()).unwrap_or_else(|e| panic!("decode with places: {e}"));
    assert_eq!(differs(&d, &back), None, "the place rows decode to the rows written");
    assert!(with[..] == bytes_of(&back)[..], "the decoded place rows re-encode equal");
    // The projection column is the last: its one row the last 13 bytes, the kind first.
    let mut kind = with[..].to_vec();
    let at = kind.len() - 13;
    kind[at] = 3;
    match decode(&kind, Alloc::global()) {
        Err(CheckedRecordError::Tag { column: "capture_projs", row: 0, tag: 3 }) => {}
        other => panic!("TWIN: a projection of kind 3 decoded: {:?}", other.map(|_| ())),
    }
    let mut past = back;
    past.capture_places.push((ExprId(1), PatId(0), 0, 2, CaptureKind::ByRef, PTy(0)));
    match decode(&bytes_of(&past), Alloc::global()) {
        Err(CheckedRecordError::ListOutside { column: "capture_places", row: 1, start: 0, len: 2 }) => {}
        other => panic!("TWIN: a place whose run passes its column decoded: {:?}", other.map(|_| ())),
    }
}

/// The record is read in place: each expression's type and the call's callee from the bytes,
/// equal to the check's.
#[test]
fn the_record_is_read_in_place() {
    let c = a_call();
    let bytes = bytes_of(&c);
    let rec = CheckedRecord::open(&bytes).unwrap_or_else(|e| panic!("open: {e}"));
    for (i, &t) in c.expr_ty.iter().enumerate() {
        assert_eq!(rec.expr_ty(ExprId(i as u32)), Some(t), "expression {i}");
    }
    assert_eq!(rec.expr_ty(ExprId(c.expr_ty.len() as u32)), None, "control: no row past the column");
    let e = c.callees.column::<0>()[0];
    assert_eq!(rec.callee_of(e), Some((c.callees.column::<1>()[0], c.callees.column::<2>()[0])));
    let none = ExprId(u32::MAX);
    assert_eq!(rec.callee_of(none), None, "control: an expression with no callee row");
}

/// Each lang item's tag decodes to itself (the record's `Lang` questions); control: a byte
/// past the list decodes to none.
#[test]
fn every_lang_tag_decodes_to_its_item() {
    let mut seen = String::new();
    for (i, &l) in LangItem::ALL.iter().enumerate() {
        assert_eq!(l as usize, i, "LangItem::ALL is not in declaration order at {i} ({l:?})");
        assert_eq!(LangItem::of_tag(i as u8), Some(l));
        seen = format!("{l:?}");
    }
    assert_eq!(seen, "From", "the list ends at the last variant");
    assert_eq!(LangItem::of_tag(LangItem::ALL.len() as u8), None, "control: a tag past the list");
}

/// A record written with the previous layout magic (`bzchkd06`) is rejected cleanly.
#[test]
fn previous_magic_bzchkd06_is_rejected() {
    let h = blaze_own::testing::heap();
    let mut old = bytes_of(&a_call());
    old[..8].copy_from_slice(b"bzchkd06");
    assert!(matches!(decode(&old, h.alloc()), Err(CheckedRecordError::Magic)));
}
