//! The trap table (record.rs `TrapRow`): rows added in any order seal to one form by offset and
//! read back row for row, `trap_at` finds each by its offset and nothing between, and a row off
//! the code, a second row at one offset, or a site naming the form's word for none is refused.

use blaze_own::{AVec, Alloc, Vec};

use crate::record::{CodeRecord, CodeView, Held, RecordError, TrapKind, TrapRow};

const BRK_UNREACHABLE: u32 = 0xD420_0000 | (0xff02 << 5);
const RET: u32 = 0xD65F_03C0;

fn sealed(rows: &[TrapRow], alloc: Alloc<'static>) -> Result<AVec<'static, u8>, RecordError> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [RET, BRK_UNREACHABLE, RET, BRK_UNREACHABLE] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    r.traps.extend_from_slice(rows);
    let s = r.seal()?;
    let mut out = Vec::new_in(alloc);
    s.encode(&mut out);
    Ok(out)
}

#[test]
fn traps_seal_by_offset_and_read_back() {
    let alloc = blaze_own::testing::heap().alloc();
    let arm = TrapRow { at: 4, kind: TrapKind::MatchNoArm, site: Some((0x1234_5678_9abc_def0_0fed_cba9_8765_4321, 17)), held: Held::Unknown, subject: None };
    let end = TrapRow { at: 12, kind: TrapKind::BodyEnd, site: None, held: Held::Unknown, subject: None };
    let one = sealed(&[end, arm], alloc).unwrap();
    let other = sealed(&[arm, end], alloc).unwrap();
    assert_eq!(one[..], other[..], "the order rows were added in leaves no trace (rule 25)");
    let view = CodeView::read(&one).unwrap();
    assert_eq!(view.traps(), 2);
    assert_eq!(view.trap(0), arm);
    assert_eq!(view.trap(1), end);
    assert_eq!(view.trap_at(4), Some(arm));
    assert_eq!(view.trap_at(12), Some(end));
    assert_eq!(view.trap_at(8), None, "a `ret` has no row");
    assert_eq!(view.trap_at(0), None);
    assert_eq!(view.trap_at(16), None);
    assert_eq!(&one[..8], b"bzcode07");
}

#[test]
fn a_bad_trap_row_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let row = |at| TrapRow { at, kind: TrapKind::LetElse, site: None, held: Held::Unknown, subject: None };
    assert_eq!(sealed(&[row(16)], alloc).err(), Some(RecordError::TrapOutside { at: 16, code: 16 }));
    assert_eq!(sealed(&[row(6)], alloc).err(), Some(RecordError::TrapOutside { at: 6, code: 16 }));
    assert_eq!(sealed(&[row(4), row(4)], alloc).err(), Some(RecordError::TrapsTwice { at: 4 }));
    let none_word = TrapRow { at: 4, kind: TrapKind::LetElse, site: Some((1, u32::MAX)), held: Held::Unknown, subject: None };
    assert_eq!(sealed(&[none_word], alloc).err(), Some(RecordError::TrapSite { at: 4 }));
}

#[test]
fn an_unknown_trap_kind_is_refused_on_read() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut bytes = sealed(&[TrapRow { at: 4, kind: TrapKind::Intrinsic, site: None, held: Held::Unknown, subject: None }], alloc).unwrap();
    // The trap rows follow the head (64), the code (16) and no relro, target, site, word or pad.
    let kind_byte = 64 + 16 + 4;
    assert_eq!(bytes[kind_byte], TrapKind::Intrinsic as u8);
    bytes[kind_byte] = 200;
    assert_eq!(CodeView::read(&bytes).err(), Some(crate::record::DecodeError::TrapKind { row: 0, byte: 200 }));
}
