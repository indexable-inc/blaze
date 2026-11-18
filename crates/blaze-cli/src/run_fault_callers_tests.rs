//! The callers a fault's line ends with (run_fault.rs `callers_said`, run schema 3): each
//! return address the child named is said by its symbol's row with the return address's
//! offset, innermost first; one in no placed code is said by its address as such; past the
//! eight named, the count; the walk's end always; no walk on the host, nothing. The line is
//! written into a buffer on the stack (no heap in this file).

use core::fmt::{self, Write as _};

use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{Callers, Fault, MOST_CALLERS, NAMED_CALLERS, Placed, Signal, Spot, WalkEnd};
use blaze_launch::wire::run::Symbol;
use blaze_typeck::ItemKey;

use super::{Named, Places, SiteShown, report};

const STRUCK: [u8; 32] = [0x66; 32];
/// `from_size_align_unchecked`'s caller and that one's caller, by their symbols' first bytes.
const ARRAY_INNER: [u8; 32] = [0xa1; 32];
const RAW_VEC: [u8; 32] = [0xb2; 32];

/// Names the two callers' symbols by items 0xa1 and 0xb2, every other symbol as having no row.
struct World;

impl Places for World {
    fn named(&mut self, symbol: [u8; 32]) -> Named {
        match symbol {
            ARRAY_INNER => Named::Item(ItemKey(0xa1)),
            RAW_VEC => Named::Item(ItemKey(0xb2)),
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

/// A line on the stack.
struct Line {
    bytes: [u8; 4096],
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
    fn new() -> Line {
        Line { bytes: [0; 4096], len: 0 }
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("the line is text")
    }
}

fn spot(symbol: [u8; 32], offset: u64) -> Spot {
    Spot { placed: Placed::Code, symbol: Symbol(symbol), record: RecordId([symbol[0] ^ 0xff; 32]), offset }
}

/// The report's line for a trap at +0x3b8 in the struck fn with `callers`.
fn said(line: &mut Line, callers: Option<Callers>) {
    let at = spot(STRUCK, 0x3b8);
    let fault = Fault { signal: Signal::Trap, pc: Some(0x1000), at: Some(at), code: None, regs: None, address: 0x1000, touched: Some(at), write: None, callers };
    let report = report(fault, &mut World, None);
    write!(line, "{report}").expect("the line fits");
}

/// Two named callers, one in no placed code, the end; then the full 32 (eight said, "and 24
/// more"); no caller walked; no walk at all. Prints each line.
#[test]
fn the_callers_are_named_innermost_first() {
    let mut three = Callers::none(WalkEnd::Top);
    three.len = 3;
    three.ret[..3].copy_from_slice(&[0x7000_0054, 0x7000_21c0, 0x1a2b3c]);
    three.at[0] = Some(spot(ARRAY_INNER, 0x54));
    three.at[1] = Some(spot(RAW_VEC, 0x1c0));
    let mut line = Line::new();
    said(&mut line, Some(three));
    let text = line.text();
    std::println!("{text}");
    assert!(
        text.ends_with(
            "; called from fn of item 0xa1 (the names give it neither a path nor a place) +0x54, from fn of item 0xb2 (the names give it neither a path nor a place) +0x1c0, from 0x1a2b3c, in no placed code (the launcher's own, or a stub) (the walk ended: the chain's end)"
        ),
        "{text}"
    );
    assert!(text.starts_with("blaze run: the program was killed by SIGTRAP: it struck at +0x3b8"), "{text}");

    let mut full = Callers::none(WalkEnd::Cap);
    full.len = MOST_CALLERS as u8;
    for (i, ret) in full.ret.iter_mut().enumerate() {
        *ret = 0x7000_0000 + 4 * i as u64;
    }
    for spot_at in full.at.iter_mut() {
        *spot_at = Some(spot(ARRAY_INNER, 0x10));
    }
    let mut line = Line::new();
    said(&mut line, Some(full));
    let text = line.text();
    std::println!("{text}");
    assert_eq!(text.matches("fn of item 0xa1").count(), NAMED_CALLERS, "{text}");
    assert!(text.ends_with(", and 24 more (the walk ended: 32 frames, the most a fault carries)"), "{text}");

    let mut line = Line::new();
    said(&mut line, Some(Callers::none(WalkEnd::NoBounds)));
    let text = line.text();
    std::println!("{text}");
    assert!(text.ends_with("; no caller was walked (the walk ended: the thread's stack bounds were not known)"), "{text}");

    let mut line = Line::new();
    said(&mut line, None);
    let text = line.text();
    assert!(!text.contains("called from") && !text.contains("walk"), "a host that walks none says nothing of it: {text}");
}
