//! The function's own data in the record form's columns, through the seam, by both tiers (piece
//! (d): the Fast tier's classes "own data aligned past 8 bytes" and "a source position in the
//! function's own data", lifted by `bzcode08`'s `code_align`/`relro_align` and source-position
//! slots; blaze-tier record/form.rs):
//!
//! - ALIGNMENT, RUN: `f(_) -> &ROW`, `ROW` a 64-aligned row behind a 5-byte one (data_align_tests.rs's
//!   function). Each tier's record asks 64 of the part holding the row; placed where the record
//!   asks, the row's address is a multiple of 64 and its bytes read back. A page-aligned row is
//!   taken the same way; 2^13 is refused by both tiers by the one reason ([`DATA_ALIGN_PAST`]).
//! - THE CONTROLS (rule 23): the same record placed 16 bytes past where it asks is refused by the
//!   placer (`Misaligned`); the record with its alignments set back to the least before the seal
//!   is placed there and the row lands off 64. The column is what puts the row on 64.
//! - SOURCE POSITIONS, RUN: `f() -> u32` loading a row's line field (a 4-aligned row of no
//!   address, which both tiers put in the code part) and a column field (an 8-aligned row holding
//!   an address: the relro part). Each tier's record holds the field as a slot (item, site,
//!   field), its four bytes zero whatever the LIR row held; placed with a number, the run returns
//!   that number. THE CONTROL: placed with another number it returns the other, and with none the
//!   placer refuses by `PositionBytes`: the slot is where the run's number comes from.
//!
//! The evaluator is not asked: a position is a placement's number, which it does not model.
//! Prints nothing; passes as `test tier_tests::own_data::... ok`.

use blaze_code::place::{At, PlaceError};
use blaze_code::record::{LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, PositionField, PositionPart, Symbol};
use blaze_inproc::exec::Executable;
use blaze_lir::builder::{DataPosition, DataReloc, MemAttrs};
use blaze_lir::{Flags, Ty};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{encode, place_resolved, read, seal};
use blaze_tier::{Emitted, Table, TierError, TierName};

use super::{Nameless, POLL_PAGE_ROOM, PollOnly, TierFn, carve_poll_page, fast, reference, table};
use crate::pass::DATA_ALIGN_PAST;
use crate::tests::one;

/// Both tiers, by name.
const TIERS: [(TierName, TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];

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

/// The item the source positions are of (any key: nothing resolves it here), and the byte of
/// its text they name.
const ITEM: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
const SITE: u32 = 17;

/// What the LIR rows hold where a position goes: the tiers must not copy it.
const PLANTED: u32 = 0xdead_beef;

/// `f(_) -> &ROW`, `ROW` aligned to `2^align`, after a 5-byte row the code reaches too (so the
/// aligned one is not first by luck).
fn returns_row(align: u32) -> blaze_lir::Func<impl core::alloc::Allocator + Clone> {
    one(&[Ty::I64], Ty::PTR, |b, v| {
        let small = b.data(&[1, 2, 3, 4, 5], 0, &[]);
        let aligned = b.data(&ROW, align, &[]);
        let first = b.data_addr(small);
        let _ = b.ptr_add(Flags::NONE, first, v[0]);
        b.data_addr(aligned)
    })
}

/// `f() -> u32`: the `field` position of a row, loaded. `in_relro` gives the row an address
/// (of a byte row), which puts it in the relro part; otherwise it is 4-aligned with no address
/// and goes to the code part.
fn returns_position(field: blaze_lir::PositionField, in_relro: bool) -> blaze_lir::Func<impl core::alloc::Allocator + Clone> {
    one(&[], Ty::I32, |b, _| {
        let mut bytes = [0u8; 16];
        bytes[8..12].copy_from_slice(&PLANTED.to_le_bytes());
        let position = [DataPosition { offset: 8, item: ITEM, site: SITE, field }];
        let row = match in_relro {
            true => {
                let text = b.data(b"src/lib.rs", 0, &[]);
                b.data_with_positions(&bytes, 3, &[DataReloc { offset: 0, target: text, addend: 0 }], &position)
            }
            false => b.data_with_positions(&bytes, 2, &[], &position),
        };
        let at = b.data_addr(row);
        b.load(Ty::I32, Flags::NONE, at, 8, MemAttrs::aligned(2))
    })
}

/// `e` sealed and encoded.
fn sealed(e: Emitted<'static>, tier: TierName) -> AVec<'static, u8> {
    let sealed = seal(e).unwrap_or_else(|error| panic!("the {} tier's record does not seal: {error}", tier.as_str()));
    let mut bytes: AVec<'static, u8> = Vec::new_in(Alloc::global());
    encode(&sealed, &mut bytes);
    bytes
}

/// `record` placed by the one new placer with `values` as its slots' numbers: its code and its
/// relro part each `shift` bytes past a multiple of the alignment the record asks of it. The
/// mapping and the function's address, or the placer's refusal.
fn placed(record: &[u8], tier: TierName, values: &[u8], shift: usize) -> Result<(Executable, u64), PlaceError> {
    let view = read(record).unwrap_or_else(|error| panic!("the {} tier's record does not read back: {error}", tier.as_str()));
    let (code_len, relro_len) = (view.code().len(), view.relro().len());
    let (code_align, relro_align) = (1usize << view.code_align(), 1usize << view.relro_align());
    let symbol = Symbol::of_name("blaze-emit tier_tests::own_data: the function under test");
    let mut done: Result<u64, PlaceError> = Ok(0);
    let exec = Executable::written(code_len + code_align + shift, POLL_PAGE_ROOM + relro_len + relro_align + shift, |code, data| {
        let (page, rest) = carve_poll_page(data);
        let c = (code_align - code.as_ptr() as usize % code_align) % code_align + shift;
        let r = (relro_align - rest.as_ptr() as usize % relro_align) % relro_align + shift;
        let code = &mut code[c..c + code_len];
        let relro = &mut rest[r..r + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = PollOnly { page };
        done = place_resolved(&view, symbol, at, &mut words, code, relro, values).map(|()| at.code);
    })
    .expect("the code maps");
    done.map(|entry| (exec, entry))
}

/// The placed function at `entry` called with `arg`.
fn call(exec: &Executable, entry: u64, arg: u64) -> u64 {
    debug_assert!(entry >= exec.code_address());
    // SAFETY: `entry` is the first byte of a placed C function of at most one integer parameter
    // returning at most one integer, inside `exec`'s mapping, which is executable while `exec`
    // lives (the caller holds it across the call).
    let f = unsafe { core::mem::transmute::<usize, extern "C" fn(u64) -> u64>(entry as usize) };
    f(arg)
}

/// ALIGNMENT, RUN, the bound and THE CONTROLS (the module's documentation).
#[test]
fn both_tiers_place_an_aligned_row_where_its_record_asks() {
    let machine = table().head().machine;
    for (tier, emit) in TIERS {
        let name = tier.as_str();
        for align in [6u32, 12] {
            let func = returns_row(align);
            let emitted = emit(func.view(), &Nameless, machine, Alloc::global()).unwrap_or_else(|error| panic!("the {name} tier refused a 2^{align}-aligned row: {error}"));
            let record = sealed(emitted, tier);
            let view = read(&record).unwrap_or_else(|error| panic!("the {name} tier's record does not read back: {error}"));
            let asks = view.code_align().max(view.relro_align());
            assert_eq!(u32::from(asks), align, "the {name} tier's record asks 2^{align} of the part holding the row");
            let (exec, entry) = placed(&record, tier, &[], 0).unwrap_or_else(|error| panic!("the {name} tier's record does not place: {error}"));
            let at = call(&exec, entry, 0);
            assert_eq!(at % (1 << align), 0, "the {name} tier: the 2^{align}-aligned row at {at:#x}");
            // SAFETY: `at` is the address of the 64-byte row inside `exec`'s mapping (the
            // function returned its `data_addr`), readable while `exec` lives.
            let bytes = unsafe { core::slice::from_raw_parts(at as *const u8, 64) };
            assert_eq!(bytes, &ROW[..], "the {name} tier: the row's bytes read back");
            drop(exec);
        }

        // THE CONTROLS: placed off what it asks, refused; asking the least, placed off 64.
        let func = returns_row(6);
        let emitted = emit(func.view(), &Nameless, machine, Alloc::global()).unwrap_or_else(|error| panic!("the {name} tier refused a 64-aligned row: {error}"));
        let record = sealed(emitted, tier);
        match placed(&record, tier, &[], 16) {
            Err(PlaceError::Misaligned { align: 64, .. }) => {}
            Err(error) => panic!("the {name} tier's record placed 16 past 64: the wrong refusal: {error}"),
            Ok(_) => panic!("the {name} tier's record was placed 16 past the 64 it asks"),
        }
        let mut least = emit(func.view(), &Nameless, machine, Alloc::global()).unwrap_or_else(|error| panic!("the {name} tier refused a 64-aligned row: {error}"));
        least.code_align = LEAST_CODE_ALIGN;
        least.relro_align = LEAST_RELRO_ALIGN;
        let record = sealed(least, tier);
        let (exec, entry) = placed(&record, tier, &[], 16).unwrap_or_else(|error| panic!("the control: the {name} tier's least-aligned record does not place: {error}"));
        let at = call(&exec, entry, 0);
        assert_ne!(at % 64, 0, "the control: the {name} tier's record asking the least, the row at {at:#x} is still on 64");
        drop(exec);

        // The bound: past a page, refused by the one reason.
        let past = returns_row(13);
        match emit(past.view(), &Nameless, machine, Alloc::global()) {
            Err(TierError::Refused { tier: by, what, .. }) if by == tier && what == DATA_ALIGN_PAST => {}
            Err(error) => panic!("the {name} tier, a 2^13-aligned row: the wrong refusal: {error}"),
            Ok(_) => panic!("the {name} tier took a row aligned past a page"),
        }
    }
}

/// SOURCE POSITIONS, RUN, and THE CONTROL (the module's documentation).
#[test]
fn both_tiers_leave_a_source_position_to_the_placer() {
    let machine = table().head().machine;
    for (tier, emit) in TIERS {
        let name = tier.as_str();
        for (lir_field, field, in_relro) in [
            (blaze_lir::PositionField::Line, PositionField::Line, false),
            (blaze_lir::PositionField::Col, PositionField::Col, true),
        ] {
            let func = returns_position(lir_field, in_relro);
            let emitted = emit(func.view(), &Nameless, machine, Alloc::global()).unwrap_or_else(|error| panic!("the {name} tier refused a row holding a {field:?} position: {error}"));
            let record = sealed(emitted, tier);
            let view = read(&record).unwrap_or_else(|error| panic!("the {name} tier's record does not read back: {error}"));
            assert_eq!(view.position_slots(), 1, "the {name} tier's record holds the one position as a slot");
            let slot = view.position_slot(0);
            assert_eq!((slot.item, slot.site, slot.field), (ITEM, SITE, field), "the {name} tier's slot names the item, the site and the field");
            let want_part = match in_relro {
                true => PositionPart::Relro,
                false => PositionPart::Code,
            };
            assert_eq!(slot.part, want_part, "the {name} tier: a {field:?} row's part");
            let part = match slot.part {
                PositionPart::Code => view.code(),
                PositionPart::Relro => view.relro(),
            };
            assert_eq!(&part[slot.at as usize..slot.at as usize + 4], &[0; 4], "the {name} tier's record holds zero where the position goes, not the row's bytes");
            for number in [42u32, 1000] {
                let (exec, entry) = placed(&record, tier, &number.to_le_bytes(), 0).unwrap_or_else(|error| panic!("the {name} tier's record does not place: {error}"));
                let got = call(&exec, entry, 0) as u32;
                assert_eq!(got, number, "the {name} tier: the {field:?} the placer gave");
                drop(exec);
            }
            match placed(&record, tier, &[], 0) {
                Err(PlaceError::PositionBytes { rows: 1, bytes: 0, .. }) => {}
                Err(error) => panic!("the control: the {name} tier's record placed with no number: the wrong refusal: {error}"),
                Ok(_) => panic!("the control: the {name} tier's record was placed with no number for its slot"),
            }
        }
    }
}
