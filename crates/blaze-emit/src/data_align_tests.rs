//! Data rows aligned past 16 (`vzipq_u8`'s 64-aligned table; the tempo census refused eight
//! fns with "data aligned past 16 bytes"): the row is placed at a multiple of its alignment
//! from its part's first byte, and the part asks that much of its placement, in both forms
//! (lib.rs [`crate::Code::align`]; blaze-code record.rs `code_align`, `relro_align`).
//!
//! - The literal-word form, RUN: a function returning the address of an align(64) row behind a
//!   5-byte one; the address is `% 64 == 0` and its 64 bytes read back.
//! - The record form: the same function's record asks `code_align` 6, round-trips through
//!   `seal`/`encode`/`CodeView::read`, and blaze-code's `lay_out` puts the row on 64 after a
//!   member that ends off 64. A row holding an address (the relro part) asks `relro_align` 6.
//! - The bound: a page-aligned row (2^12) is taken, 2^13 refused by name.
//! - THE CONTROL (rule 23): the record's `code_align` set back to the least (16) before it is
//!   sealed, the row lands off 64: the field is what puts it there.
//!
//! The record form's allocations are in an inline arena on the stack (`Inline` through
//! `Erased`); the literal-word form uses the shared helpers (tests.rs).

#![cfg(target_arch = "aarch64")]

use blaze_code::image::{Kind, Member, lay_out};
use blaze_code::record::{CodeRecord, CodeView, Symbol};
use blaze_lir::{Flags, Ty, builder::DataReloc};
use blaze_own::{AVec, Alloc, Erased, Inline, Vec};

use crate::{
    EmitError, Named, Names, Outside, RoutinePlaces, emit_record,
    pass::DATA_ALIGN_PAST,
    tests::{ISA, emitted, one, run},
};

/// The 64 bytes of the aligned row: `0..64`.
const ROW: [u8; 64] = {
    let mut row = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        row[i] = i as u8;
        i += 1;
    }
    row
};

/// `f(_) -> &ROW`, `ROW` a row aligned to `2^align`, after a 5-byte row the code reaches too
/// (so the aligned one is not first by luck).
fn returns_row(align: u32) -> blaze_lir::Func<impl core::alloc::Allocator + Clone> {
    one(&[Ty::I64], Ty::PTR, |b, v| {
        let small = b.data(&[1, 2, 3, 4, 5], 0, &[]);
        let aligned = b.data(&ROW, align, &[]);
        let first = b.data_addr(small);
        let _ = b.ptr_add(Flags::NONE, first, v[0]);
        b.data_addr(aligned)
    })
}

/// No reference here has a name (the function names nothing outside itself).
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// The literal-word form, run: the row's address is a multiple of 64, its bytes read back.
///
/// Prints: `test data_align_tests::an_align_64_row_runs_on_64 ... ok`
#[test]
fn an_align_64_row_runs_on_64() {
    let func = returns_row(6);
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("an align(64) row emits: {e}"));
    assert_eq!(code.align, 6, "the code asks 64 of its placement");
    let exec = run::place(&code, |_| unreachable!("no literal word"));
    let at = run::call(&exec, [0, 0, 0, 0]);
    assert_eq!(at % 64, 0, "the row at {at:#x}");
    assert_eq!((at - exec.code_address()) % 64, 0, "a multiple of 64 from the code's first byte");
    // SAFETY: `at` is the address of the 64-byte row inside `exec`'s mapping (the function
    // returned its `data_addr`), readable while `exec` lives.
    let bytes = unsafe { core::slice::from_raw_parts(at as *const u8, 64) };
    assert_eq!(bytes, &ROW[..], "the row's bytes read back");
}

/// `func` in the record form, sealed and encoded, with `code_align` overwritten by `force`
/// when given (the control).
fn record_bytes<'a>(func: blaze_lir::View<'_>, force: Option<u8>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let (mut record, _): (CodeRecord<'a>, _) =
        emit_record(blaze_lir::verify(func).expect("the test's function is LIR"), &Nameless, &RoutinePlaces::NONE, ISA, alloc).unwrap_or_else(|e| panic!("the record emits: {e:?}"));
    if let Some(align) = force {
        record.code_align = align;
    }
    let sealed = record.seal().unwrap_or_else(|e| panic!("the record seals: {e}"));
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// A record of one `ret` (4 bytes of code), at the least alignments.
fn filler<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut r = CodeRecord::new_in(alloc);
    r.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
    let sealed = r.seal().expect("the filler seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// The offset of `ROW` in `code`, which holds it once.
fn row_in(code: &[u8]) -> u64 {
    let at = code.windows(64).position(|w| w == ROW).expect("the row is in the code");
    at as u64
}

/// The record form: `code_align` 6, a round trip, the row on 64 from the code's first byte,
/// and on 64 in a segment after the filler; THE CONTROL with the field forced to 16.
///
/// Prints: `test data_align_tests::the_record_asks_its_alignment_and_lay_out_gives_it ... ok`
#[test]
fn the_record_asks_its_alignment_and_lay_out_gives_it() {
    let inline = Inline::<262144>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let func = returns_row(6);
    let bytes = record_bytes(func.view(), None, alloc);
    let view = CodeView::read(&bytes).unwrap_or_else(|e| panic!("reads back: {e}"));
    assert_eq!(view.code_align(), 6, "the record asks 64 of its code's placement");
    let offset = row_in(view.code());
    assert_eq!(offset % 64, 0, "the row at code offset {offset}");
    let first = filler(alloc);
    for (force, on_64) in [(None, true), (Some(4u8), false)] {
        let bytes = record_bytes(func.view(), force, alloc);
        let members = [
            Member { symbol: Symbol([1; 32]), record: CodeView::read(&first).expect("reads") },
            Member { symbol: Symbol([2; 32]), record: CodeView::read(&bytes).expect("reads") },
        ];
        let segment = lay_out(&members, Kind::Delta, 16384, alloc).unwrap_or_else(|e| panic!("lays out: {e}"));
        let row = segment.homes[1].1 + offset;
        assert_eq!(&segment.code[row as usize..row as usize + 64], &ROW[..]);
        match on_64 {
            true => assert_eq!(row % 64, 0, "in the segment the row at {row:#x} is on 64"),
            // The control: the field removed, the row follows the filler at 16 and is off 64.
            false => assert_ne!(row % 64, 0, "the control: at the least alignment the row at {row:#x} is off 64"),
        }
    }
}

/// A 64-aligned row holding an address goes to the relro part, which then asks 64.
///
/// Prints: `test data_align_tests::a_relro_row_asks_its_alignment ... ok`
#[test]
fn a_relro_row_asks_its_alignment() {
    let inline = Inline::<262144>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let func = one(&[Ty::I64], Ty::PTR, |b, _| {
        let text = b.data(b"hi!", 0, &[]);
        let table = b.data(&[0; 64], 6, &[DataReloc { offset: 0, target: text, addend: 1 }]);
        b.data_addr(table)
    });
    let bytes = record_bytes(func.view(), None, alloc);
    let view = CodeView::read(&bytes).unwrap_or_else(|e| panic!("reads back: {e}"));
    assert_eq!(view.relro_align(), 6, "the relro part asks 64");
    assert_eq!(view.relro().len() % 64, 0, "the 64-byte row from relro offset 0");
}

/// The bound: a page-aligned row is taken in both forms; 2^13 is refused naming the bound.
///
/// Prints: `test data_align_tests::a_page_aligned_row_is_taken_and_past_a_page_refused ... ok`
#[test]
fn a_page_aligned_row_is_taken_and_past_a_page_refused() {
    let page = returns_row(12);
    let code = emitted(page.view(), &[]).unwrap_or_else(|e| panic!("a page-aligned row emits: {e}"));
    assert_eq!(code.align, 12);
    let exec = run::place(&code, |_| unreachable!("no literal word"));
    let at = run::call(&exec, [0, 0, 0, 0]);
    assert_eq!(at % 4096, 0, "the row at {at:#x} is on a 4 KiB page");
    let past = returns_row(13);
    let got = emitted(past.view(), &[]).map(|_| ());
    assert!(matches!(got, Err(EmitError::Unsupported { why, .. }) if why == DATA_ALIGN_PAST), "2^13: {got:?}");
}
