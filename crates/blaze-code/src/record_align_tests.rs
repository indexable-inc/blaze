//! The parts' alignments (record.rs `code_align`, `relro_align`, form `bzcode06`): they encode
//! and read back; a record of the older form `bzcode05` is refused naming it (rule 26); an
//! alignment out of range is refused when sealed and when read; `lay_out` puts a 64-aligned
//! record's code and relro part on multiples of 64 after a member that ends off one, and
//! `place` refuses an address that is not. THE CONTROL (rule 23): the same record asking the
//! least alignment (16) is laid out where its 64-aligned row is NOT on a multiple of 64.
//!
//! Every allocation is in an inline arena on the stack (`Inline` through `Erased`).

use blaze_own::{AVec, Alloc, Erased, Inline, Vec};

use crate::image::{ImageError, Kind, Member, lay_out};
use crate::place::{At, PlaceError, Words, place};
use crate::record::{
    CodeRecord, CodeView, DecodeError, LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, MAGIC, MOST_PART_ALIGN, OWN_RELRO, PositionField, PositionPart,
    PositionRow, RecordError, Symbol, Target, Word,
};

const RET: u32 = 0xD65F_03C0;

/// A function of one `ret`, then (at code offset 64) 64 bytes `0..64`, a row aligned to 64;
/// its relro part a 64-aligned row of one word naming its own relro byte 8. `code_align` and
/// `relro_align` as given.
fn aligned<'a>(code_align: u8, relro_align: u8, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, RecordError> {
    let mut r = CodeRecord::new_in(alloc);
    r.code.extend_from_slice(&RET.to_le_bytes());
    r.code.resize(64, 0);
    for b in 0..64u8 {
        r.code.push(b);
    }
    r.relro.resize(64, 0);
    r.words.push(Word { at: 0, target: OWN_RELRO, addend: 8 });
    r.code_align = code_align;
    r.relro_align = relro_align;
    let sealed = r.seal()?;
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    Ok(out)
}

/// A function of one `ret`, at the least alignments: 4 bytes of code, so the next member's
/// code would start at 16 with nothing asking more.
fn filler<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut r = CodeRecord::new_in(alloc);
    r.code.extend_from_slice(&RET.to_le_bytes());
    r.relro.resize(8, 0);
    r.words.push(Word { at: 0, target: OWN_RELRO, addend: 0 });
    let sealed = r.seal().expect("the filler seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

fn sym(n: u8) -> Symbol {
    Symbol([n; 32])
}

/// Prints: `test record_align_tests::alignments_round_trip_and_older_forms_are_refused ... ok`
#[test]
fn alignments_round_trip_and_older_forms_are_refused() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let bytes = aligned(6, 6, alloc).expect("seals");
    assert_eq!(&bytes[..8], &MAGIC);
    assert_eq!(&MAGIC, b"bzcode07");
    assert_eq!(&bytes[12..16], &[6, 6, 0, 0], "the two alignments where `bzcode05` held a zero `flags`");
    let view = CodeView::read(&bytes).expect("reads back");
    assert_eq!((view.code_align(), view.relro_align()), (6, 6));
    assert_eq!(&view.code()[64..], &core::array::from_fn::<u8, 64, _>(|i| i as u8)[..], "the row's bytes read back");
    // Equal records, equal bytes (rule 25): a second build writes the same.
    assert_eq!(aligned(6, 6, alloc).expect("seals")[..], bytes[..]);
    let least = aligned(LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, alloc).expect("seals");
    let view = CodeView::read(&least).expect("reads back");
    assert_eq!((view.code_align(), view.relro_align()), (LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN));

    // The older form, by name: its head said `bzcode05` and held no alignments.
    let mut older = Vec::new_in(alloc);
    older.extend_from_slice(&bytes);
    older[..8].copy_from_slice(b"bzcode05");
    older[12..16].fill(0);
    let refused = CodeView::read(&older).err();
    assert_eq!(refused, Some(DecodeError::Older { form: *b"bzcode05" }));
    let said = format!("{}", refused.expect("refused"));
    assert!(said.contains("bzcode05") && said.contains("rebuilt"), "the refusal names the form: {said}");
    older[..8].copy_from_slice(b"bzcode04");
    assert_eq!(CodeView::read(&older).err(), Some(DecodeError::Older { form: *b"bzcode04" }));
    older[..8].copy_from_slice(b"bzcode06");
    assert_eq!(CodeView::read(&older[..56]).err(), Some(DecodeError::Older { form: *b"bzcode06" }), "even an empty v6 record is identified");
    older[..8].copy_from_slice(b"notacode");
    assert_eq!(CodeView::read(&older).err(), Some(DecodeError::Magic), "bytes of no form of this record");

    // Out of range: refused when sealed, and when read from bytes that say so.
    assert_eq!(aligned(3, 3, alloc).err(), Some(RecordError::Align { part: "code", align: 3 }), "code below 16");
    assert_eq!(aligned(13, 3, alloc).err(), Some(RecordError::Align { part: "code", align: 13 }), "code past a page");
    assert_eq!(aligned(4, 2, alloc).err(), Some(RecordError::Align { part: "relro part", align: 2 }), "relro below 8");
    assert_eq!(aligned(4, MOST_PART_ALIGN + 1, alloc).err(), Some(RecordError::Align { part: "relro part", align: 13 }));
    let mut bad = Vec::new_in(alloc);
    bad.extend_from_slice(&bytes);
    bad[12] = 13;
    assert_eq!(CodeView::read(&bad).err(), Some(DecodeError::Align { part: "code", align: 13 }));
    bad[12] = 6;
    bad[13] = 2;
    assert_eq!(CodeView::read(&bad).err(), Some(DecodeError::Align { part: "relro part", align: 2 }));
    bad[13] = 6;
    bad[15] = 1;
    assert_eq!(CodeView::read(&bad).err(), Some(DecodeError::Align { part: "head's zero bytes", align: 1 }));
}

fn positioned<'a>(code: &[u8], relro: &[u8], rows: &[PositionRow], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, RecordError> {
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(code);
    record.relro.extend_from_slice(relro);
    record.positions.extend_from_slice(rows);
    let mut bytes = Vec::new_in(alloc);
    record.seal()?.encode(&mut bytes);
    Ok(bytes)
}

const LINE: PositionRow = PositionRow {
    part: PositionPart::Relro,
    at: 0,
    item: 0x1234_5678_9abc_def0_fedc_ba98_7654_3210,
    site: 37,
    field: PositionField::Line,
};

#[test]
fn position_slots_round_trip_in_part_and_offset_order() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let code = [0; 4];
    let relro = [0; 8];
    let code_row = PositionRow { part: PositionPart::Code, ..LINE };
    let col = PositionRow { at: 4, field: PositionField::Col, ..LINE };
    let bytes = positioned(&code, &relro, &[col, LINE, code_row], alloc).unwrap();
    let view = CodeView::read(&bytes).unwrap();
    assert_eq!(view.positions(), 3);
    let decoded = [view.position(0), view.position(1), view.position(2)];
    assert_eq!(decoded, [code_row, LINE, col]);
    assert_eq!(view.code(), &code);
    assert_eq!(view.relro(), &relro);
    let rebuilt = positioned(view.code(), view.relro(), &decoded, alloc).unwrap();
    assert_eq!(rebuilt.as_slice(), bytes.as_slice());
    assert_eq!(view.require_positionless("test image without source"), Err(RecordError::UnresolvedPosition {
        operation: "test image without source", row: code_row,
    }));
}

#[test]
fn duplicate_unaligned_outside_and_nonzero_position_slots_are_refused() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let part = LINE.part;
    let col = PositionRow { at: 4, field: PositionField::Col, ..LINE };
    for (at, error) in [
        (0, RecordError::PositionsTwice { part, at: 0 }),
        (2, RecordError::PositionMisaligned { part, at: 2 }),
        (8, RecordError::PositionOutside { part, at: 8, len: 8 }),
    ] {
        let bad = PositionRow { at, ..col };
        assert_eq!(positioned(&[], &[0; 8], &[LINE, bad], alloc).err(), Some(error));
        // Corrupt the same offset in bytes already stored, bypassing the builder.
        let mut bytes = positioned(&[], &[0; 8], &[LINE, col], alloc).unwrap();
        let offset = bytes.len() - 32 + 20;
        bytes[offset..offset + 4].copy_from_slice(&at.to_le_bytes());
        assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Position(error)));
    }
    let absolute = 41u32.to_le_bytes();
    let error = RecordError::PositionNonzero { part, at: 0 };
    assert_eq!(positioned(&[], &absolute, &[LINE], alloc).err(), Some(error));
    let mut bytes = positioned(&[], &[0; 4], &[LINE], alloc).unwrap();
    bytes[64..68].copy_from_slice(&absolute);
    assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Position(error)));
}

#[test]
fn a_source_position_cannot_share_a_reference_word() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let row = PositionRow { at: 4, ..LINE };
    let make = |position| {
        let mut record = CodeRecord::new_in(alloc);
        record.relro.resize(16, 0);
        record.words.push(Word { at: 0, target: OWN_RELRO, addend: 8 });
        record.positions.push(position);
        record.seal()
    };
    let error = RecordError::PositionOverlapsReference { part: row.part, at: 4 };
    assert_eq!(make(row).err(), Some(error));
    let mut bytes = Vec::new_in(alloc);
    make(PositionRow { at: 8, ..row }).unwrap().encode(&mut bytes);
    let offset = bytes.len() - 32 + 20;
    bytes[offset..offset + 4].copy_from_slice(&4u32.to_le_bytes());
    assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Position(error)));
}

/// `lay_out` after a 4-byte member: the 64-aligned record's code and its row on multiples of
/// 64, its relro part too; and THE CONTROL: at the least alignment the row lands off 64.
///
/// Prints: `test record_align_tests::lay_out_places_each_part_at_its_alignment ... ok`
#[test]
fn lay_out_places_each_part_at_its_alignment() {
    let inline = Inline::<262144>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let first = filler(alloc);
    let wide = aligned(6, 6, alloc).expect("seals");
    let narrow = aligned(LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, alloc).expect("seals");
    for (bytes, want_64) in [(&wide, true), (&narrow, false)] {
        let members = [
            Member { symbol: sym(1), record: CodeView::read(&first).expect("reads") },
            Member { symbol: sym(2), record: CodeView::read(bytes).expect("reads") },
        ];
        let segment = lay_out(&members, Kind::Delta, 16384, alloc).expect("lays out");
        let home = segment.homes[1].1;
        let row = home + 64;
        // The filler's relro part is 8 bytes from `relro_at`; the second's follows it.
        let (second_word, _, _) = segment.words[1];
        let relro = segment.relro_at + second_word;
        match want_64 {
            true => {
                assert_eq!(home % 64, 0, "the code at {home:#x} is on 64");
                assert_eq!(row % 64, 0, "the 64-aligned row at {row:#x} is on 64");
                assert_eq!(relro % 64, 0, "the relro part at {relro:#x} is on 64");
                assert_eq!(&segment.code[row as usize..row as usize + 64], &core::array::from_fn::<u8, 64, _>(|i| i as u8)[..]);
            }
            // The control: the field removed (the least asked), the row is off 64.
            false => {
                assert_eq!(home, 16, "at the least alignment the code follows the filler at 16");
                assert_ne!(row % 64, 0, "the control: without the field the row is not on 64");
            }
        }
    }
}

/// Answers no target: the record placed here has no site and no word naming one.
struct NoWords;

impl Words for NoWords {
    fn word(&mut self, target: Target, _: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }
}

/// `place` refuses code or a relro part off its record's alignment, naming the part and the
/// alignment, and places it on one.
///
/// Prints: `test record_align_tests::place_refuses_an_address_off_the_alignment ... ok`
#[test]
fn place_refuses_an_address_off_the_alignment() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let bytes = aligned(6, 6, alloc).expect("seals");
    let view = CodeView::read(&bytes).expect("reads");
    let mut code = [0u8; 128];
    let mut relro = [0u8; 64];
    let symbol = sym(2);
    let off = place(&view, symbol, At { code: 0x10_0010, relro: 0x20_0000 }, &mut NoWords, &mut code, &mut relro);
    assert_eq!(off, Err(PlaceError::Misaligned { symbol, what: "the code", at: 0x10_0010, align: 64 }));
    let off = place(&view, symbol, At { code: 0x10_0040, relro: 0x20_0008 }, &mut NoWords, &mut code, &mut relro);
    assert_eq!(off, Err(PlaceError::Misaligned { symbol, what: "the relro part", at: 0x20_0008, align: 64 }));
    place(&view, symbol, At { code: 0x10_0040, relro: 0x20_0040 }, &mut NoWords, &mut code, &mut relro).expect("places on 64");
    assert_eq!(&code[64..], &core::array::from_fn::<u8, 64, _>(|i| i as u8)[..]);
    assert_eq!(u64::from_le_bytes(relro[..8].try_into().expect("a word")), 0x20_0048, "the own word: relro byte 8");
}
