//! [`place`]: a record placed alone at an address reaches the run's words. Each test prints the
//! line its doc comment names.

use blaze_own::{AVec, Alloc, Vec};

use crate::a64::{adrp_add_target, adrp_ldr_target};
use crate::image::{ImageError, Kind, Member, lay_out, lay_out_positions};
use crate::place::{At, PlaceError, Words, place, place_positions, place_resolved};
use crate::record::{CodeRecord, CodeView, NO_TARGET, OWN_CODE, PositionField, PositionPart, PositionRow, Site, SiteKind, Space, Symbol, Target, Word};

const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const ADRP_X0: u32 = 0x9000_0000;
const ADD_X0: u32 = 0x9100_0000;
const RET: u32 = 0xD65F_03C0;

/// The run's reservation in these tests: the slot words at `TABLE`, the thunk words 64 KiB after
/// them, the code area 1 GiB on.
const TABLE: u64 = 0x1_0000_0000;
const THUNK_WORDS: u64 = TABLE + 0x1_0000;
const THUNKS: u64 = 0x2_0000_0000;
const CODE: u64 = TABLE + 0x4000_0000;

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: Symbol::of_name(name) }
}

/// The run's table over two functions, `f` (slot 0) and `g` (slot 1); anything else is unbound.
struct Table {
    far: bool,
}

impl Table {
    fn index(&self, target: Target) -> Result<u64, ImageError> {
        match target {
            t if t == func("f") => Ok(0),
            t if t == func("g") => Ok(1),
            _ => Err(ImageError::Unbound { target }),
        }
    }
}

impl Words for Table {
    fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
        let index = self.index(target)?;
        let base = match (self.far, call) {
            // 8 GiB away: past `adrp`'s reach.
            (true, _) => TABLE + (8 << 30),
            (false, true) => TABLE,
            (false, false) => THUNK_WORDS,
        };
        Ok(base + 8 * index)
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        Ok(THUNKS + 16 * self.index(target)?)
    }
}

/// `call f; x0 = &relro[8]; ret`, with relro words `&g + 4` (g's thunk) and its own code.
fn record(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X16, LDR_X16, BLR_X16, ADRP_X0, ADD_X0, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    r.relro.extend_from_slice(&[0; 16]);
    let f = r.target(func("f"));
    let g = r.target(func("g"));
    r.sites.push(Site { at: 0, kind: SiteKind::Call, target: f, addend: 0 });
    r.sites.push(Site { at: 12, kind: SiteKind::Relro, target: NO_TARGET, addend: 8 });
    r.words.push(Word { at: 0, target: g, addend: 4 });
    r.words.push(Word { at: 8, target: OWN_CODE, addend: 0 });
    let sealed = r.seal().unwrap();
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

fn inst(code: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// Prints `place: call reaches slot word 0, relro site reaches its byte 8, words hold g's thunk + 4 and the code`.
#[test]
fn a_placed_record_reaches_the_run_words() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(alloc);
    let view = CodeView::read(&bytes).unwrap();
    let at = At { code: CODE + 0x40, relro: CODE + 0x8000 };
    let mut code = [0u8; 24];
    let mut relro = [0u8; 16];
    place(&view, Symbol::of_name("h"), at, &mut Table { far: false }, &mut code, &mut relro).unwrap();
    let called = adrp_ldr_target(inst(&code, 0), inst(&code, 4), at.code).unwrap();
    assert_eq!(called, TABLE, "the call loads f's slot word");
    assert_eq!(inst(&code, 8), BLR_X16, "the blr stands");
    let addressed = adrp_add_target(inst(&code, 12), inst(&code, 16), at.code + 12).unwrap();
    assert_eq!(addressed, at.relro + 8, "the relro site reaches its byte");
    let first = u64::from_le_bytes(relro[0..8].try_into().unwrap());
    let second = u64::from_le_bytes(relro[8..16].try_into().unwrap());
    assert_eq!(first, THUNKS + 16 + 4, "a function's address is its thunk, never a body");
    assert_eq!(second, at.code);
    println!("place: call reaches slot word 0, relro site reaches its byte 8, words hold g's thunk + 4 and the code");
}

/// Prints `place: a word 8 GiB away is refused by name`.
#[test]
fn a_word_out_of_reach_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(alloc);
    let view = CodeView::read(&bytes).unwrap();
    let at = At { code: CODE, relro: CODE + 0x8000 };
    let mut code = [0u8; 24];
    let mut relro = [0u8; 16];
    let error = place(&view, Symbol::of_name("h"), at, &mut Table { far: true }, &mut code, &mut relro).unwrap_err();
    assert!(
        matches!(error, PlaceError::Site(ImageError::OutOfReach { symbol, .. }) if symbol == Symbol::of_name("h")),
        "{error:?}"
    );
    println!("place: a word 8 GiB away is refused by name");
}

/// Prints `place: current line and column in code and relro, missing positions refused`.
#[test]
fn positions_are_filled_only_in_the_placed_parts() {
    let alloc = blaze_own::testing::heap().alloc();
    let symbol = Symbol::of_name("caller");
    let line = PositionRow { part: PositionPart::Code, at: 4, item: 0x1234, site: 15, field: PositionField::Line };
    let col = PositionRow { part: PositionPart::Relro, at: 0, field: PositionField::Col, ..line };
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(&RET.to_le_bytes());
    record.code.extend_from_slice(&[0; 4]);
    record.relro.extend_from_slice(&[0; 8]);
    record.positions.push(col);
    record.positions.push(line);
    let mut bytes = Vec::new_in(alloc);
    record.seal().unwrap().encode(&mut bytes);
    let view = CodeView::read(&bytes).unwrap();
    let at = At { code: CODE, relro: CODE + 0x8000 };
    let mut code = [0u8; 8];
    let mut relro = [0u8; 8];
    let mut words = Table { far: false };
    let members = [Member { symbol, record: view }];
    let missing = ImageError::UnresolvedPosition { symbol, row: line };
    assert_eq!(place(&view, symbol, at, &mut words, &mut code, &mut relro), Err(PlaceError::Site(missing)));
    assert_eq!(lay_out(&members, Kind::Delta, 4096, alloc).err(), Some(missing));
    for bytes in [0, 4, 7, 9, 12] {
        assert_eq!(
            place_resolved(&view, symbol, at, &mut words, &mut code, &mut relro, &[0; 12][..bytes]),
            Err(PlaceError::PositionBytes { symbol, rows: 2, bytes }),
        );
    }
    assert_eq!(
        place_resolved(&view, symbol, at, &mut words, &mut code, &mut relro, &[1, 0, 0, 0, 0, 0, 0, 0]),
        Err(PlaceError::Site(ImageError::ZeroPosition { symbol, row: col })),
    );
    for expected_line in [7, 10] {
        let source_position = |row: PositionRow| {
            assert_eq!((row.item, row.site), (line.item, line.site));
            Ok((expected_line, 16))
        };
        place_positions(&view, symbol, at, &mut words, &mut code, &mut relro, |i, row| {
            assert_eq!(row, [line, col][i]);
            Ok(row.field.value(source_position(row)?))
        }).unwrap();
        assert_eq!(inst(&code, 4), expected_line);
        assert_eq!(inst(&relro, 0), 16);
        let segment = lay_out_positions(&members, Kind::Delta, 4096, alloc, |_, row| source_position(row)).unwrap();
        assert_eq!(segment.code.as_slice(), &code[..]);
        assert_eq!(segment.relro.as_slice(), &relro[..]);
        assert_eq!(inst(view.code(), 4), 0);
        assert_eq!(inst(view.relro(), 0), 0);
    }
    place_resolved(&view, symbol, at, &mut words, &mut code, &mut relro, &[4, 3, 2, 1, 16, 0, 0, 0]).unwrap();
    assert_eq!(inst(&code, 4), 0x0102_0304);
    assert_eq!(inst(&relro, 0), 16);
    println!("place: current line and column in code and relro, missing positions refused");
}
