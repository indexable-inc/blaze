//! The value a fault's trap read (run_fault.rs `held_value`, `subject_said`): a Discriminant
//! row whose held value is `x9`, with the child's registers, says "it read 0x3 (x9)" and names
//! the enum with the discriminants its row keeps, `..` and the count when it has more; without
//! registers it says the register and that none were sent; a row whose value is not known says
//! no value. The line is written into a buffer on the stack (no heap in this file).

use core::fmt::{self, Write as _};

use blaze_code::record::{CodeRecord, Held, SUBJECT_SHOWN, Subject, TrapKind, TrapRow};
use blaze_code::trap;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{Arm64, Fault, Placed, Regs, Signal, Spot};
use blaze_launch::wire::run::Symbol;
use blaze_own::{Erased, Inline, Vec};
use blaze_typeck::ItemKey;

use super::{Named, Places, SiteShown, report};

const FUNCTION: [u8; 32] = [0x66; 32];
const OWNER: u128 = 0x00c6_374c_4df0_69ed_4450_cf86_b5da_d7c5;
const ALIGNMENT_ENUM: u128 = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff;
const TWO: u128 = 0x0022;

/// Names every item by its key alone and places no site.
struct World;

impl Places for World {
    fn named(&mut self, _symbol: [u8; 32]) -> Named {
        Named::NoRow
    }

    fn site(&mut self, _item: ItemKey, _at: u32) -> Option<SiteShown> {
        None
    }

    fn item(&mut self, item: ItemKey) -> Named {
        Named::Item(item)
    }
}

/// A line on the stack.
struct Line {
    bytes: [u8; 2048],
    len: usize,
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).filter(|&end| end <= self.bytes.len()).ok_or(fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl Line {
    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("the line is text")
    }
}

/// `AlignmentEnum`'s row: 64 variants `1 << n`, the first eight kept.
fn alignment_enum() -> Subject {
    let mut discriminants = [0u64; SUBJECT_SHOWN];
    for (n, d) in discriminants.iter_mut().enumerate() {
        *d = 1 << n;
    }
    Subject { item: ALIGNMENT_ENUM, count: 64, discriminants }
}

/// `brk #0xff02` at 4, 8 and 12: AlignmentEnum's copy (held in x9, sited), a two-variant enum's
/// (held in x9, no site), a `match` whose value is not known.
fn record_bytes(alloc: blaze_own::Alloc<'_>) -> blaze_own::AVec<'_, u8> {
    let mut record = CodeRecord::new_in(alloc);
    let brk = 0xd420_0000 | (trap::UNREACHABLE << 5);
    for word in [0xd503_201fu32, brk, brk, brk] {
        record.code.extend_from_slice(&word.to_le_bytes());
    }
    let two = Subject { item: TWO, count: 2, discriminants: [1, 2, 0, 0, 0, 0, 0, 0] };
    record.traps.push(TrapRow { at: 4, kind: TrapKind::Discriminant, site: Some((OWNER, 40)), held: Held::Reg(9), subject: Some(alignment_enum()) });
    record.traps.push(TrapRow { at: 8, kind: TrapKind::Discriminant, site: None, held: Held::Reg(9), subject: Some(two) });
    record.traps.push(TrapRow { at: 12, kind: TrapKind::MatchNoArm, site: None, held: Held::Unknown, subject: None });
    let sealed = record.seal().expect("the record seals");
    let mut bytes = Vec::new_in(alloc);
    sealed.encode(&mut bytes);
    bytes
}

fn regs(x9: u64) -> Regs {
    let mut x = [0u64; 31];
    x[9] = x9;
    Regs::Arm64(Arm64 { x, sp: 0x16f_dff0_00, pc: 0x1004, pstate: 0x6000_0000 })
}

fn said(line: &mut Line, offset: u64, regs: Option<Regs>, bytes: &[u8]) {
    let at = Spot { placed: Placed::Code, symbol: Symbol(FUNCTION), record: RecordId([0x2d; 32]), offset };
    let fault = Fault { signal: Signal::Trap, pc: Some(0x1000 + offset), at: Some(at), code: None, regs, address: 0x1000 + offset, touched: Some(at), write: None, callers: None };
    line.len = 0;
    write!(line, "{}", report(fault, &mut World, Some(Ok(bytes)))).expect("the line fits");
}

#[test]
fn a_discriminant_trap_says_the_value_it_read_and_the_enum() {
    let inline = Inline::<8192>::new();
    let erased = Erased::new(&inline);
    let bytes = record_bytes(erased.handle());
    let mut line = Line { bytes: [0; 2048], len: 0 };

    said(&mut line, 4, Some(regs(0x3)), &bytes);
    let alignment = line.text();
    assert!(
        alignment.ends_with(
            ": the read of the enum's discriminant at byte 40 of item 0x00c6374c4df069ed4450cf86b5dad7c5 (not placed in today's text) found no variant's discriminant: it read 0x3 (x9); enum of item 0x112233445566778899aabbccddeeff (the names give it neither a path nor a place) has discriminants 0x1, 0x2, 0x4, 0x8, 0x10, 0x20, 0x40, 0x80, .. (64)"
        ),
        "{alignment}"
    );
    std::println!("{alignment}");

    said(&mut line, 8, Some(regs(0x3)), &bytes);
    let two = line.text();
    assert!(two.contains("(the build kept no site"), "a row with no site says so: {two}");
    assert!(two.contains("found no variant's discriminant: it read 0x3 (x9); enum of item 0x22"), "{two}");
    assert!(two.ends_with("has discriminants 0x1, 0x2"), "two variants, all kept, no count: {two}");

    said(&mut line, 4, None, &bytes);
    let no_regs = line.text();
    assert!(no_regs.contains(": it read the value in x9, and the program sent no registers on this host;"), "{no_regs}");

    said(&mut line, 12, Some(regs(0x3)), &bytes);
    let unknown = line.text();
    assert!(unknown.ends_with("took no arm"), "no value is said where the row knows none: {unknown}");
}
