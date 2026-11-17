//! The `bzcode02` form: a round trip of a record with every section, each list the caller sorts
//! refused unsorted by its own variant, and `from_v1` placed against blaze-code's `bzcode01`
//! `place` as the oracle. Each test prints the line its doc comment names.

extern crate std;

use std::println;

use blaze_code::a64::adrp_ldr_target;
use blaze_code::image::ImageError;
use blaze_code::place::{At, Words};
use blaze_code::record::{CodeRecord, CodeView, NO_TARGET, OWN_CODE, OWN_RELRO, Pad, Site, SiteKind, Space, Symbol, Target, Word};
use blaze_code::unwind::{Action, Frame, Row};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::InstanceKey;
use blaze_run::parent::Read;

use super::form::{FormError, encode, read, seal};
use super::place::place;
use super::v1::from_v1;
use super::{Emitted, InlineFrame, Live, LoopHeader, OUTERMOST, Place, Poll, Position, Reloc, RelocKind};
use crate::Reg;

const STP_FRAME: u32 = 0xA9BF_7BFD;
const MOV_FP: u32 = 0x9100_03FD;
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const ADRP_X0: u32 = 0x9000_0000;
const ADD_X0: u32 = 0x9100_0000;
const ADRP_X1: u32 = 0x9000_0001;
const LDR_X1: u32 = 0xF940_0021;
const ADRP_X2: u32 = 0x9000_0002;
const LDR_X2: u32 = 0xF940_0042;
const BL: u32 = 0x9400_0000;
const LDR_XZR_X16: u32 = 0xF940_021F;
const LDP_FRAME: u32 = 0xA8C1_7BFD;
const RET: u32 = 0xD65F_03C0;

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: Symbol::of_name(name) }
}

fn outside(name: &str) -> Target {
    Target { space: Space::Outside, symbol: Symbol::of_name(name) }
}

fn push_code(code: &mut AVec<'_, u8>, insts: &[u32]) {
    for inst in insts {
        code.extend_from_slice(&inst.to_le_bytes());
    }
}

/// A record with every section: a call through a GOT pair, a relro address, a direct `bl`, a
/// poll, relro words of each kind and an `Abs64`, a pad and its unwind row, a loop with three
/// live values, two inline frames, two positions, two reads, an outside name and one unused name.
/// The targets come unsorted and with a duplicate (the call names the duplicate).
fn every(alloc: Alloc<'static>) -> Emitted<'static> {
    let mut e = Emitted::new_in(alloc);
    push_code(
        &mut e.code,
        &[STP_FRAME, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X0, ADD_X0, BL, ADRP_X16, LDR_XZR_X16, LDP_FRAME, RET],
    );
    e.relro.extend_from_slice(&[0; 24]);
    e.frame = 16;
    for t in [func("g"), func("f"), Target { space: Space::Static, symbol: Symbol::of_name("poll page") }, outside("puts"), func("f")] {
        e.targets.push(t);
    }
    let (g, f_dup, poll, puts) = (0u32, 4u32, 2u32, 3u32);
    for r in [
        Reloc { at: 8, in_relro: false, kind: RelocKind::GotPage21, target: f_dup, addend: 0 },
        Reloc { at: 12, in_relro: false, kind: RelocKind::GotPageOff12, target: f_dup, addend: 0 },
        Reloc { at: 16, in_relro: true, kind: RelocKind::Abs64, target: puts, addend: 0 },
        Reloc { at: 20, in_relro: false, kind: RelocKind::Page21, target: OWN_RELRO, addend: 8 },
        Reloc { at: 24, in_relro: false, kind: RelocKind::PageOff12 { scale: 0 }, target: OWN_RELRO, addend: 8 },
        Reloc { at: 28, in_relro: false, kind: RelocKind::Branch26, target: g, addend: 0 },
        Reloc { at: 32, in_relro: false, kind: RelocKind::Page21, target: poll, addend: 0 },
        Reloc { at: 36, in_relro: false, kind: RelocKind::PageOff12 { scale: 3 }, target: poll, addend: 0 },
    ] {
        e.relocs.push(r);
    }
    e.words.push(Word { at: 8, target: OWN_CODE, addend: 0 });
    e.words.push(Word { at: 0, target: g, addend: 4 });
    e.pads.push(Pad { start: 16, len: 4, landing: 40, action: Action::Cleanup.code() });
    e.unwind_frame = Some(Frame::PLAIN);
    e.unwind.push(Row { start: 16, len: 4, landing: 40, action: Action::Cleanup });
    e.loops.push(LoopHeader { at: 20, first: 0, count: 3 });
    e.live.push(Live { value: 3, place: Place::Reg(Reg(19)) });
    e.live.push(Live { value: 7, place: Place::Cell { sp_offset: 16 } });
    e.live.push(Live { value: 9, place: Place::Slot { sp_offset: 32 } });
    e.polls.push(Poll { at: 32, header: 0 });
    e.frames.push(InlineFrame { parent: OUTERMOST, instance: InstanceKey([7; 32]), call_lo: 10, call_hi: 20 });
    e.frames.push(InlineFrame { parent: 0, instance: InstanceKey([8; 32]), call_lo: 3, call_hi: 5 });
    e.positions.push(Position { at: 0, frame: OUTERMOST, span_lo: 0, span_hi: 40 });
    e.positions.push(Position { at: 8, frame: 1, span_lo: 1, span_hi: 2 });
    e.reads.push(Read { fact: [1; 32], digest: [2; 32] });
    e.reads.push(Read { fact: [3; 32], digest: [4; 32] });
    e.text.extend_from_slice(b"unusedputs");
    e.names.push((Symbol::of_name("unused"), 0, 6));
    e.names.push((Symbol::of_name("puts"), 6, 4));
    e
}

/// Prints `bzcode02: a record with every section reads back and encodes to equal bytes`.
/// Fails if a section is not written, read or sealed alike (the second encoding differs), or if
/// the seal does not merge the duplicate target, renumber the call, or drop the unused name.
#[test]
fn every_section_round_trips_to_equal_bytes() {
    let alloc = Alloc::global();
    let sealed = seal(every(alloc)).unwrap();
    let mut first: AVec<'static, u8> = Vec::new_in(alloc);
    encode(&sealed, &mut first);
    let view = read(&first).unwrap();
    assert_eq!(view.targets(), 4, "the duplicate target is merged");
    assert_eq!(view.relocs(), 8);
    let call = view.reloc(0);
    assert_eq!(view.target(call.target as usize), func("f"), "the call's index is renumbered");
    assert_eq!(view.names(), 1, "the unused name is dropped");
    assert_eq!(view.name(Symbol::of_name("puts")), Some("puts"));
    assert_eq!(view.word(0), Word { at: 0, target: view.word(0).target, addend: 4 }, "words sorted by offset");
    assert_eq!(view.lives(), 3);
    assert_eq!(view.live(2).place, Place::Slot { sp_offset: 32 });
    assert_eq!(view.inline_frame(1).instance, InstanceKey([8; 32]));
    assert_eq!(view.read_row(1), Read { fact: [3; 32], digest: [4; 32] });
    assert_eq!(view.unwind_frame(), Some(Frame::PLAIN));
    let again = seal(view.to_emitted(alloc)).unwrap();
    let mut second: AVec<'static, u8> = Vec::new_in(alloc);
    encode(&again, &mut second);
    assert!(first[..] == second[..], "the second encoding differs");
    println!("bzcode02: a record with every section reads back and encodes to equal bytes");
}

/// Prints `bzcode02: each unsorted list is refused by its own variant`. Fails if a list the
/// caller sorts is sorted quietly or refused under another list's name (the control seals).
#[test]
fn each_unsorted_list_is_refused_by_its_own_variant() {
    let alloc = Alloc::global();
    assert!(seal(every(alloc)).is_ok(), "the control record seals");

    let mut e = every(alloc);
    e.relocs.swap(0, 1);
    assert_eq!(seal(e).err(), Some(FormError::RelocsUnsorted { at: 8 }));

    let mut e = every(alloc);
    e.loops.push(LoopHeader { at: 4, first: 0, count: 0 });
    assert_eq!(seal(e).err(), Some(FormError::LoopsUnsorted { at: 4 }));

    let mut e = every(alloc);
    e.live.swap(0, 1);
    assert_eq!(seal(e).err(), Some(FormError::LiveUnsorted { header: 0 }));

    let mut e = every(alloc);
    e.polls.push(Poll { at: 8, header: 0 });
    assert_eq!(seal(e).err(), Some(FormError::PollsUnsorted { at: 8 }));

    let mut e = every(alloc);
    e.positions.swap(0, 1);
    assert_eq!(seal(e).err(), Some(FormError::PositionsUnsorted { at: 0 }));

    let mut e = every(alloc);
    e.reads.swap(0, 1);
    assert_eq!(seal(e).err(), Some(FormError::ReadsUnsorted { row: 1 }));

    let mut e = every(alloc);
    e.reads.push(Read { fact: [3; 32], digest: [9; 32] });
    assert_eq!(seal(e).err(), Some(FormError::ReadsUnsorted { row: 2 }), "a repeated fact is refused too");
    println!("bzcode02: each unsorted list is refused by its own variant");
}

/// The run's reservation: the slot words at `TABLE`, the thunk words 64 KiB after them (another
/// page: a call and a load of one target reach different pages), the thunks, the code area.
const TABLE: u64 = 0x1_0000_0000;
const THUNK_WORDS: u64 = TABLE + 0x1_0000;
const THUNKS: u64 = 0x2_0000_0000;
const CODE: u64 = TABLE + 0x4000_0000;

/// The run's table over `f` (0), `g` (1) and the C name `puts` (2).
struct Table;

impl Table {
    fn index(target: Target) -> Result<u64, ImageError> {
        match target {
            t if t == func("f") => Ok(0),
            t if t == func("g") => Ok(1),
            t if t == outside("puts") => Ok(2),
            _ => Err(ImageError::Unbound { target }),
        }
    }
}

impl Words for Table {
    fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
        let base = if call { TABLE } else { THUNK_WORDS };
        Ok(base + 8 * Table::index(target)?)
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        Ok(THUNKS + 16 * Table::index(target)?)
    }
}

/// The Reference tier's kind of record, every site kind once: `call f; x1 = &g; x0 = &relro[8];
/// x2 = &puts; ret`, with relro words `&g + 4`, its own code and `&puts`.
fn v1_record(alloc: Alloc<'static>) -> CodeRecord<'static> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X16, LDR_X16, BLR_X16, ADRP_X1, LDR_X1, ADRP_X0, ADD_X0, ADRP_X2, LDR_X2, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    r.relro.extend_from_slice(&[0; 24]);
    let f = r.target(func("f"));
    let g = r.target(func("g"));
    let puts = r.outside("puts");
    r.sites.push(Site { at: 0, kind: SiteKind::Call, target: f, addend: 0 });
    r.sites.push(Site { at: 12, kind: SiteKind::Load, target: g, addend: 0 });
    r.sites.push(Site { at: 20, kind: SiteKind::Relro, target: NO_TARGET, addend: 8 });
    r.sites.push(Site { at: 28, kind: SiteKind::Load, target: puts, addend: 0 });
    r.words.push(Word { at: 0, target: g, addend: 4 });
    r.words.push(Word { at: 8, target: OWN_CODE, addend: 0 });
    r.words.push(Word { at: 16, target: puts, addend: 0 });
    r
}

fn inst(code: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// Prints `bzcode02: from_v1 places to the bytes bzcode01's place makes (call, load, relro, words)`.
/// The oracle is blaze-code's `bzcode01` `place`. Fails if one site kind maps wrong: a load or
/// call made `Page21` + `PageOff12` reaches the thunk instead of the word, and a call not seen as
/// a call loads the thunk word instead of the slot word.
#[test]
fn from_v1_places_like_bzcode01() {
    let alloc = Alloc::global();
    let at = At { code: CODE + 0x40, relro: CODE + 0x8000 };
    let symbol = Symbol::of_name("h");

    let sealed = v1_record(alloc).seal().unwrap();
    let mut old_bytes: AVec<'static, u8> = Vec::new_in(alloc);
    sealed.encode(&mut old_bytes);
    let old_view = CodeView::read(&old_bytes).unwrap();
    let mut old_code = [0u8; 40];
    let mut old_relro = [0u8; 24];
    blaze_code::place::place(&old_view, symbol, at, &mut Table, &mut old_code, &mut old_relro).unwrap();

    let emitted = from_v1(v1_record(alloc), alloc).unwrap();
    let sealed = seal(emitted).unwrap();
    let mut new_bytes: AVec<'static, u8> = Vec::new_in(alloc);
    encode(&sealed, &mut new_bytes);
    let view = read(&new_bytes).unwrap();
    let mut code = [0u8; 40];
    let mut relro = [0u8; 24];
    place(&view, symbol, at, &mut Table, &mut code, &mut relro).unwrap();

    assert_eq!(adrp_ldr_target(inst(&old_code, 0), inst(&old_code, 4), at.code), Some(TABLE), "the oracle's call loads f's slot word");
    assert_eq!(adrp_ldr_target(inst(&old_code, 12), inst(&old_code, 16), at.code + 12), Some(THUNK_WORDS + 8), "the oracle's load reaches g's thunk word");
    assert!(code == old_code, "code differs from bzcode01's place");
    assert!(relro == old_relro, "relro differs from bzcode01's place");
    println!("bzcode02: from_v1 places to the bytes bzcode01's place makes (call, load, relro, words)");
}
