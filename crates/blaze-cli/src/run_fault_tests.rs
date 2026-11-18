//! The named line (run_fault.rs `report`), pure: a fault as the child tells it, the names the
//! walk gives, and the bytes of the record the pc is in.

use blaze_code::record::{CodeRecord, Site, SiteKind, Space, Symbol as CodeSymbol, Target};
use blaze_code::trap;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{Fault, Placed, Signal, Spot};
use blaze_launch::wire::run::Symbol;
use blaze_own::{Erased, Inline, Vec};
use blaze_typeck::ItemKey;

use super::{Named, Places, SiteShown, report};

const FUNCTION: [u8; 32] = [0x66; 32];
const CALLEE: [u8; 32] = [0x5a; 32];

/// FUNCTION is item 0x1234; nothing else has a row; no site is placed.
struct Names;

impl Places for Names {
    fn named(&mut self, symbol: [u8; 32]) -> Named {
        match symbol {
            FUNCTION => Named::Item(ItemKey(0x1234)),
            _ => Named::NoRow,
        }
    }

    fn site(&mut self, _item: ItemKey, _at: u32) -> Option<SiteShown> {
        None
    }

    fn item(&mut self, item: ItemKey) -> Named {
        Named::Item(item)
    }
}

/// The coordinator's std-hello stop in its shape (2026-11-17 21:42: SIGTRAP at +0x3fc): the
/// pc's word is `brk #0xff02` right after a call site, so the line says the reason
/// (`unreachable`), the call it follows and that the call returned; the pc's own place is said
/// once (a trap's address is its pc). A pc at an abort says "an abort" and no call; a pc in no
/// placed function says so. Prints the first line.
#[test]
fn a_trap_is_said_by_its_reason_and_the_call_it_ends() {
    let inline = Inline::<16384>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    let mut record = CodeRecord::new_in(alloc);
    // 0xfc nops (to 0x3f0), then adrp x16; ldr x16, [x16]; blr x16 (the call at 0x3f0);
    // brk #0xff02 at 0x3fc; brk #0xfe00 at 0x400.
    for _ in 0..0xfc {
        record.code.extend_from_slice(&0xd503_201fu32.to_le_bytes());
    }
    for word in [0x9000_0010u32, 0xf940_0210, 0xd63f_0200, 0xd420_0000 | (trap::UNREACHABLE << 5), 0xd420_0000 | (trap::ABORT << 5)] {
        record.code.extend_from_slice(&word.to_le_bytes());
    }
    let target = record.target(Target { space: Space::Func, symbol: CodeSymbol(CALLEE) });
    record.sites.push(Site { at: 0x3f0, kind: SiteKind::Call, target, addend: 0 });
    let sealed = record.seal().expect("the record seals");
    let mut bytes = Vec::new_in(alloc);
    sealed.encode(&mut bytes);

    let at = Spot { placed: Placed::Code, symbol: Symbol(FUNCTION), record: RecordId([0x2d; 32]), offset: 0x3fc };
    let fault = Fault { signal: Signal::Trap, pc: Some(0x70_02ae_a76c), at: Some(at), address: 0x70_02ae_a76c, touched: Some(at), write: None, code: None, regs: None, callers: None };
    let said = std::format!("{}", report(fault, &mut Names, Some(Ok(bytes.as_slice()))));
    assert!(said.starts_with("blaze run: the program was killed by SIGTRAP: it struck at +0x3fc in fn of item 0x1234"), "{said}");
    assert!(said.contains("`brk #0xff02`: an `unreachable` reached"), "{said}");
    assert!(said.contains("right after the call of a fn this run has no row for") && said.contains("(symbol 5a5a5a5a5a5a)"), "{said}");
    assert!(said.contains("that call returned"), "{said}");
    assert!(!said.contains("; the address"), "the pc's place was said twice: {said}");

    let abort = Spot { offset: 0x400, ..at };
    let fault = Fault { at: Some(abort), touched: Some(abort), ..fault };
    let aborted = std::format!("{}", report(fault, &mut Names, Some(Ok(bytes.as_slice()))));
    assert!(aborted.contains("`brk #0xfe00`: an abort") && !aborted.contains("right after the call"), "{aborted}");

    let nowhere = Fault { signal: Signal::Segv, pc: Some(0x10), at: None, address: 0x20, touched: None, write: Some(true), code: None, regs: None, callers: None };
    let lost = std::format!("{}", report(nowhere, &mut Names, None));
    assert_eq!(lost, "blaze run: the program was killed by SIGSEGV: its pc 0x10 is in no placed function");
    std::println!("{said}");
}
