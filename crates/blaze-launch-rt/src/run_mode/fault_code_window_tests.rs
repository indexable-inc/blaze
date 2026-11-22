//! Which code bytes the crash handler reads around the pc (fault_report.rs `code_window`): 16
//! before and 16 from the pc, clipped to the placed code row and to the pc's 4 KiB page, and
//! none for a pc in no code row or for a fault that is the fetch of the pc's own page (which
//! may not be mapped: reading it in the handler would fault again).

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::RecordId;
use blaze_sys::posix::signal::{SIGSEGV, SIGTRAP};

use super::fault_report::{Kind, Row, Struck, code_window};

const AREA: u64 = 0x70_0000_6000;

fn trap(pc: u64) -> Struck {
    Struck { sig: SIGTRAP, pc: Some(pc), address: pc, write: None }
}

/// The std-hello stop's shape (+0x3fc in a function's code): the whole 32 bytes; a pc 8 bytes
/// into its row: 8 before it; a pc 4 bytes before its row's end: 4 from it; a pc 8 bytes past
/// a page edge: from the edge; a pc in a datum or in no row: none; a SIGSEGV at the pc's own
/// page (an instruction fetch): none, and one at another page (a data access): the window.
#[test]
fn the_code_window_stays_in_its_row_and_its_page() {
    let f = Row::new(Kind::Code, AREA + 0x100, 0x800, CodeSymbol([0x66; 32]), RecordId([0xd9; 32]));
    let d = Row::new(Kind::ReadOnlyDatum, AREA + 0x40, 0x40, CodeSymbol([0x51; 32]), RecordId([0x77; 32]));
    let rows = [d, f];
    let pc = AREA + 0x100 + 0x3fc;
    assert_eq!(code_window(trap(pc), &rows), Some((pc - 16, 32)));
    assert_eq!(code_window(trap(AREA + 0x108), &rows), Some((AREA + 0x100, 24)));
    assert_eq!(code_window(trap(AREA + 0x8fc), &rows), Some((AREA + 0x8ec, 20)));
    // AREA + 0x1000 is a 4 KiB page edge inside a longer row.
    let long = Row::new(Kind::Code, AREA + 0x100, 0x2000, CodeSymbol([0x67; 32]), RecordId([0xda; 32]));
    assert_eq!(code_window(trap(AREA + 0x1008), &[long]), Some((AREA + 0x1000, 24)));
    assert_eq!(code_window(trap(AREA + 0xffc), &[long]), Some((AREA + 0xfec, 20)));
    assert_eq!(code_window(trap(AREA + 0x48), &rows), None, "a pc in a datum");
    assert_eq!(code_window(trap(AREA + 0x4000), &rows), None, "a pc in no row");
    assert_eq!(code_window(Struck { sig: SIGTRAP, pc: None, address: 0, write: None }, &rows), None);
    let fetch = Struck { sig: SIGSEGV, pc: Some(pc), address: pc, write: None };
    assert_eq!(code_window(fetch, &rows), None, "the fetch of the pc's page");
    let data = Struck { sig: SIGSEGV, pc: Some(pc), address: 0x10, write: Some(false) };
    assert_eq!(code_window(data, &rows), Some((pc - 16, 32)));
}
