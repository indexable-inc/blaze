//! A trap row's held value and subject (record.rs `Held`, `Subject`, form `bzcode05`): each
//! reads back as written whatever order the rows were added in; a subject's discriminants past
//! the ones it keeps seal to 0 (one form, rule 25); a register past x30 is refused when sealed;
//! a malformed held tag and a subject naming no trap row are refused when read.

use blaze_own::{AVec, Alloc, Erased, Inline, Vec};

use crate::record::{CodeRecord, CodeView, DecodeError, Held, RecordError, SUBJECT_SHOWN, Subject, TrapKind, TrapRow};

const BRK_UNREACHABLE: u32 = 0xD420_0000 | (0xff02 << 5);
const RET: u32 = 0xD65F_03C0;

fn sealed<'a>(rows: &[TrapRow], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, RecordError> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [RET, BRK_UNREACHABLE, RET, BRK_UNREACHABLE, BRK_UNREACHABLE] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    r.traps.extend_from_slice(rows);
    let s = r.seal()?;
    let mut out = Vec::new_in(alloc);
    s.encode(&mut out);
    Ok(out)
}

/// `AlignmentEnum`'s shape: 64 variants `1 << n`, the first eight kept.
fn alignment_enum() -> Subject {
    let mut discriminants = [0u64; SUBJECT_SHOWN];
    for (n, d) in discriminants.iter_mut().enumerate() {
        *d = 1 << n;
    }
    Subject { item: 0x00ab_cdef_0123_4567_89ab_cdef_0123_4567, count: 64, discriminants }
}

#[test]
fn held_values_and_subjects_read_back_in_one_form() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let copy = TrapRow { at: 4, kind: TrapKind::Discriminant, site: Some((0x77, 40)), held: Held::Reg(9), subject: Some(alignment_enum()) };
    // A two-variant enum whose unkept discriminants were left non-zero by its builder.
    let two = Subject { item: 5, count: 2, discriminants: [1, 2, 9, 9, 9, 9, 9, 9] };
    let small = TrapRow { at: 12, kind: TrapKind::Discriminant, site: None, held: Held::Stack(0x1f0), subject: Some(two) };
    let arm = TrapRow { at: 16, kind: TrapKind::MatchNoArm, site: None, held: Held::Unknown, subject: None };
    let one = sealed(&[arm, small, copy], alloc).unwrap();
    let other = sealed(&[copy, arm, small], alloc).unwrap();
    assert_eq!(one[..], other[..], "the order rows were added in leaves no trace (rule 25)");
    assert_eq!(&one[..8], b"bzcode07");
    let view = CodeView::read(&one).unwrap();
    assert_eq!(view.traps(), 3);
    assert_eq!(view.trap_at(4), Some(copy));
    let read = view.trap_at(12).expect("the row at 12");
    assert_eq!(read.held, Held::Stack(0x1f0));
    assert_eq!(read.subject.map(|s| s.discriminants), Some([1, 2, 0, 0, 0, 0, 0, 0]), "unkept discriminants seal to 0");
    assert_eq!(read.subject.map(|s| s.shown().len()), Some(2));
    assert_eq!(view.trap_at(16), Some(arm));
    assert_eq!(alignment_enum().shown().len(), SUBJECT_SHOWN, "64 variants: eight kept");
}

#[test]
fn a_bad_held_value_or_subject_is_refused() {
    let inline = Inline::<65536>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let x31 = TrapRow { at: 4, kind: TrapKind::Discriminant, site: None, held: Held::Reg(31), subject: None };
    assert_eq!(sealed(&[x31], alloc).err(), Some(RecordError::HeldReg { at: 4, reg: 31 }));

    let row = TrapRow { at: 4, kind: TrapKind::Discriminant, site: None, held: Held::Reg(9), subject: Some(alignment_enum()) };
    let mut bytes = sealed(&[row], alloc).unwrap();
    // The trap row follows the head (64) and the code (20, padded to 24); its held tag is its
    // byte 5, and the subject row follows it (32 bytes).
    let trap = 64 + 24;
    assert_eq!(bytes[trap + 5], 1);
    assert_eq!(bytes[trap + 6], 9);
    bytes[trap + 5] = 3;
    assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Held { row: 0 }));
    bytes[trap + 5] = 1;
    bytes[trap + 6] = 31;
    assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Held { row: 0 }));
    bytes[trap + 6] = 9;
    assert!(CodeView::read(&bytes).is_ok());
    // The subject's offset made 8: no trap row is there.
    let subject = trap + 32;
    assert_eq!(bytes[subject], 4);
    bytes[subject] = 8;
    assert_eq!(CodeView::read(&bytes).err(), Some(DecodeError::Subject { row: 0 }));
}
