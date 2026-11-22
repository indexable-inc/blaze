//! WHICH `unreachable` (run_fault.rs `report`, the record's trap table): a `brk #0xff02` whose
//! trap row names a `match` at a site the walk places is said as "the `match` at F:L:C took no
//! arm"; the same row with a site nothing places names the item and byte; a row with no site
//! names the construct alone; a `brk` with no row says only the reason.

use blaze_code::record::{CodeRecord, Held, TrapKind, TrapRow};
use blaze_code::trap;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{Fault, Placed, Signal, Spot};
use blaze_launch::wire::run::Symbol;
use blaze_own::{Erased, Inline, Vec};
use blaze_typeck::ItemKey;

use super::{Named, Places, SiteShown, report};

const FUNCTION: [u8; 32] = [0x66; 32];
const OWNER: u128 = 0x00c6_374c_4df0_69ed_4450_cf86_b5da_d7c5;

/// Places byte 7 of OWNER when `placing`; names FUNCTION by a path.
struct World {
    placing: bool,
}

impl Places for World {
    fn named(&mut self, symbol: [u8; 32]) -> Named {
        match symbol {
            FUNCTION => Named::Path(Box::from("core::fmt::write")),
            _ => Named::NoRow,
        }
    }

    fn site(&mut self, item: ItemKey, at: u32) -> Option<SiteShown> {
        (self.placing && item == ItemKey(OWNER) && at == 7).then(|| SiteShown { file: Box::from("core/src/fmt/mod.rs"), line: 1234, column: 9 })
    }

    fn item(&mut self, item: ItemKey) -> Named {
        Named::Item(item)
    }
}

/// The record: `nop`, then `brk #0xff02` at 4, 8 and 12; trap rows at 4 (a `match`, sited) and
/// 8 (a `let`-`else`, no site); none at 12.
fn record_bytes(alloc: blaze_own::Alloc<'_>) -> blaze_own::AVec<'_, u8> {
    let mut record = CodeRecord::new_in(alloc);
    let brk = 0xd420_0000 | (trap::UNREACHABLE << 5);
    for word in [0xd503_201fu32, brk, brk, brk] {
        record.code.extend_from_slice(&word.to_le_bytes());
    }
    record.traps.push(TrapRow { at: 8, kind: TrapKind::LetElse, site: None, held: Held::Unknown, subject: None });
    record.traps.push(TrapRow { at: 4, kind: TrapKind::MatchNoArm, site: Some((OWNER, 7)), held: Held::Unknown, subject: None });
    let sealed = record.seal().expect("the record seals");
    let mut bytes = Vec::new_in(alloc);
    sealed.encode(&mut bytes);
    bytes
}

fn said(offset: u64, placing: bool, bytes: &[u8]) -> std::string::String {
    let at = Spot { placed: Placed::Code, symbol: Symbol(FUNCTION), record: RecordId([0x2d; 32]), offset };
    let fault = Fault { signal: Signal::Trap, pc: Some(0x1000 + offset), at: Some(at), address: 0x1000 + offset, touched: Some(at), write: None, code: None, regs: None, callers: None };
    std::format!("{}", report(fault, &mut World { placing }, Some(Ok(bytes))))
}

#[test]
fn an_unreachable_is_said_by_the_construct_it_ends() {
    let inline = Inline::<4096>::new();
    let erased = Erased::new(&inline);
    let bytes = record_bytes(erased.handle());

    let placed = said(4, true, &bytes);
    assert!(placed.contains("in fn `core::fmt::write`"), "{placed}");
    assert!(
        placed.ends_with("`brk #0xff02`: an `unreachable` reached (code the compiler holds is never run): the `match` at core/src/fmt/mod.rs:1234:9 took no arm"),
        "{placed}"
    );

    let unplaced = said(4, false, &bytes);
    assert!(unplaced.contains(": the `match` at byte 7 of item 0x00c6374c4df069ed4450cf86b5dad7c5 (not placed in today's text) took no arm"), "{unplaced}");

    let no_site = said(8, true, &bytes);
    assert!(no_site.ends_with(": the `let`-`else` (the build kept no site: it was made outside any of the body's expressions, or that expression has no range in its item's text) ran its `else` to the end (it must diverge)"), "{no_site}");

    let no_row = said(12, true, &bytes);
    assert!(no_row.ends_with("an `unreachable` reached (code the compiler holds is never run)"), "{no_row}");
    std::println!("{placed}");
}
