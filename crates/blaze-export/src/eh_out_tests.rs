//! The unwind tables read back by a small reader written here from the formats' own texts, not
//! from eh_out.rs: an FDE must cover each function with a frame, a landing function's FDE must
//! point into `except_tab` at a call-site table equal to its rows, and each platform's index
//! must lead an unwinder to that FDE.
//!
//! The rules the expectations come from:
//! - The Rust Reference, "Panic", "Unwinding": "Panicking may either be recoverable or
//!   non-recoverable ... a panic unwinds the stack ... running destructors". For that, the
//!   unwinder must find, from a return address inside a function, the function's frame rules
//!   and its landing for that call: the FDE whose `[pc_begin, pc_begin + pc_range)` holds it,
//!   and the LSDA it points to.
//! - The pinned library's personality routine, library/std/src/sys/personality/dwarf/eh.rs
//!   `find_eh_action` (nightly-2026-10-27): "The return address points 1 byte past the call
//!   instruction", the call-site table "is sorted by cs_start", `if cs_lpad == 0 { return
//!   Ok(EHAction::None) }`, `if cs_action_entry == 0 { EHAction::Cleanup(lpad) }`, and the
//!   landing is `lpad_base + cs_lpad` with `lpad_base` the function's start
//!   (`_Unwind_GetRegionStart`, which is the FDE's `pc_begin`).
//! - LSB 5.0 "Exception Frames": a CIE's id is 0, an FDE's CIE pointer counts back from its
//!   own field, `z` brings an augmentation length, `P` the personality's encoding and pointer,
//!   `L` the FDEs' LSDA encoding, `R` their pc encoding; `.eh_frame_hdr` is version 1 with a
//!   binary-search table of (initial location, FDE address) sorted by initial location.
//! - `<mach-o/compact_unwind_encoding.h>`: `UNWIND_ARM64_MODE_DWARF` (0x03000000) with the
//!   FDE's offset in `__eh_frame` in the low 24 bits; regular second-level pages
//!   (`UNWIND_SECOND_LEVEL_REGULAR`, 2) of (function offset, encoding).

use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_code::unwind::{self, Action, Frame, Row};
use blaze_own::{AVec, Alloc, Vec};

use super::{COMPACT_DWARF, COMPACT_FRAME, EhSections, ROOM_SLACK, TABLES_ALIGN, eh_room, eh_sections};
use crate::error::ExportError;
use crate::export::{Panics, export};
use crate::input::{Entry, Function, Panic, Platform, ProgramIn, Tier};
use crate::layout::{Laid, Member, lay_out};
use crate::{check, elf, macho};

const STP: u32 = 0xA9BF_7BFD; // stp x29, x30, [sp, #-16]!
const MOV_FP: u32 = 0x9100_03FD; // mov x29, sp
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const MOV_W0_1: u32 = 0x5280_0020;
const MOV_W0_42: u32 = 0x5280_0540;
const B_PLUS_8: u32 = 0x1400_0002;
const SUB_SP_32: u32 = 0xD100_83FF; // sub sp, sp, #32
const STP_ZR_0: u32 = 0xA900_7FFF; // stp xzr, xzr, [sp]
const STP_ZR_16: u32 = 0xA901_7FFF; // stp xzr, xzr, [sp, #16]
const MOV_X0_SP: u32 = 0x9100_03E0; // mov x0, sp
const BRK: u32 = 0xD420_0000;
const RET: u32 = 0xD65F_03C0;

/// `main`'s one row: the call of `thrower` (its `adrp; ldr; blr` site, which the layout makes
/// `nop; nop; bl`), landing at the `mov w0, #42` that leads to `exit(42)`.
const MAIN_ROW: Row = Row { start: 8, len: 12, landing: 28, action: Action::Cleanup };

fn sym(name: &str) -> Symbol {
    Symbol::of_name(name)
}

/// A sealed, encoded record of `code` with sites `(at, kind, target)`, calls of C names, and
/// (when `frame` is given) its unwind rows.
fn rec(code: &[u32], sites: &[(u32, SiteKind, Target)], outside: &[(u32, &str)], frame: Option<Frame>, rows: &[Row]) -> AVec<'static, u8> {
    let alloc = Alloc::global();
    let mut r = CodeRecord::new_in(alloc);
    for &inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, kind, target) in sites {
        let index = r.target(target);
        r.sites.push(Site { at, kind, target: index, addend: 0 });
    }
    for &(at, name) in outside {
        let index = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    if let Some(frame) = frame {
        unwind::fill(&mut r, frame, rows).unwrap();
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// `main` (plain frame, one cleanup row), `thrower` (plain frame, no rows: raises a zeroed
/// exception through `_Unwind_RaiseException`), `personality` (no frame description).
fn records() -> [AVec<'static, u8>; 3] {
    let main = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, MOV_W0_1, B_PLUS_8, MOV_W0_42, ADRP_X16, LDR_X16, BLR_X16],
        &[(8, SiteKind::Call, Target { space: Space::Func, symbol: sym("thrower") })],
        &[(32, "exit")],
        Some(Frame::PLAIN),
        &[MAIN_ROW],
    );
    let thrower = rec(
        &[STP, MOV_FP, SUB_SP_32, STP_ZR_0, STP_ZR_16, MOV_X0_SP, ADRP_X16, LDR_X16, BLR_X16, BRK],
        &[],
        &[(24, "_Unwind_RaiseException")],
        Some(Frame::PLAIN),
        &[],
    );
    let personality = rec(&[0x5280_00C0, RET], &[], &[], None, &[]);
    [main, thrower, personality]
}

fn functions_of<'r>(records: &'r [AVec<'static, u8>; 3]) -> [Function<'r>; 3] {
    let names = ["main", "thrower", "personality"];
    [0, 1, 2].map(|k| Function { symbol: sym(names[k]), record: CodeView::read(&records[k]).unwrap(), tier: Tier::Fast })
}

fn program<'r>(functions: &'r [Function<'r>], personality: Option<Symbol>) -> ProgramIn<'r> {
    ProgramIn { name: "unwinds", functions, data: &[], entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality, tls: None, profile: None, natives: None, exports: &[] }
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn uleb(b: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0u32);
    loop {
        let byte = b[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return value;
        }
    }
}

fn sleb(b: &[u8], at: &mut usize) -> i64 {
    let (mut value, mut shift) = (0i64, 0u32);
    loop {
        let byte = b[*at];
        *at += 1;
        value |= i64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                value |= -1i64 << shift;
            }
            return value;
        }
    }
}

/// A `pcrel|sdata4` pointer at `at` of a section placed at image offset `section_at`.
fn pcrel(b: &[u8], at: usize, section_at: u64) -> u64 {
    (section_at as i64 + at as i64 + i64::from(get32(b, at) as i32)) as u64
}

/// What a CIE says: its personality (image offset) and whether its FDEs carry an LSDA.
struct CieRead {
    personality: Option<u64>,
    lsda: bool,
}

fn read_cie(b: &[u8], at: usize, section_at: u64) -> CieRead {
    assert_eq!(get32(b, at + 4), 0, "a CIE's id is 0");
    assert_eq!(b[at + 8], 1, "CIE version 1");
    let mut p = at + 9;
    let aug_at = p;
    while b[p] != 0 {
        p += 1;
    }
    let aug_end = p;
    p += 1;
    assert_eq!(uleb(b, &mut p), 4, "code alignment factor");
    assert_eq!(sleb(b, &mut p), -8, "data alignment factor");
    assert_eq!(uleb(b, &mut p), 30, "return address register x30");
    assert_eq!(b[aug_at], b'z', "augmentation starts with z");
    let _len = uleb(b, &mut p);
    let mut cie = CieRead { personality: None, lsda: false };
    for k in aug_at + 1..aug_end {
        match b[k] {
            b'P' => {
                assert_eq!(b[p], 0x1b, "personality encoding pcrel|sdata4");
                cie.personality = Some(pcrel(b, p + 1, section_at));
                p += 5;
            }
            b'L' => {
                assert_eq!(b[p], 0x1b, "LSDA encoding pcrel|sdata4");
                cie.lsda = true;
                p += 1;
            }
            b'R' => {
                assert_eq!(b[p], 0x1b, "FDE pc encoding pcrel|sdata4");
                p += 1;
            }
            other => panic!("augmentation letter {other:#x} not written by eh_out"),
        }
    }
    cie
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FdeRead {
    offset: u32,
    pc_begin: u64,
    pc_range: u64,
    personality: Option<u64>,
    lsda: Option<u64>,
}

/// Every FDE of an `eh_frame` at image offset `section_at`, up to its zero terminator.
fn read_eh_frame(b: &[u8], section_at: u64) -> AVec<'static, FdeRead> {
    let mut fdes = Vec::new_in(Alloc::global());
    let mut at = 0usize;
    loop {
        let len = get32(b, at) as usize;
        if len == 0 {
            assert_eq!(at + 4, b.len(), "the zero terminator ends eh_frame");
            return fdes;
        }
        let id = get32(b, at + 4) as usize;
        if id != 0 {
            let cie = read_cie(b, at + 4 - id, section_at);
            let pc_begin = pcrel(b, at + 8, section_at);
            let pc_range = u64::from(get32(b, at + 12));
            let mut p = at + 16;
            let aug = uleb(b, &mut p);
            let lsda = match cie.lsda {
                true => {
                    assert_eq!(aug, 4, "an FDE with an LSDA has 4 bytes of augmentation");
                    Some(pcrel(b, p, section_at))
                }
                false => {
                    assert_eq!(aug, 0, "an FDE without an LSDA has none");
                    None
                }
            };
            fdes.push(FdeRead { offset: at as u32, pc_begin, pc_range, personality: cie.personality, lsda });
        }
        at += 4 + len;
    }
}

/// The call-site rows of the LSDA at `at` in `except_tab`: `(start, len, lpad, action entry)`.
fn read_lsda(b: &[u8], at: usize) -> AVec<'static, (u32, u32, u32, u64)> {
    let mut p = at;
    assert_eq!(b[p], 0xff, "landings are relative to the function's start (DW_EH_PE_omit)");
    p += 1;
    if b[p] != 0xff {
        p += 1;
        let _types = uleb(b, &mut p);
    } else {
        p += 1;
    }
    assert_eq!(b[p], 0x03, "call sites in udata4");
    p += 1;
    let table = uleb(b, &mut p) as usize;
    let end = p + table;
    let mut rows = Vec::new_in(Alloc::global());
    while p < end {
        let (start, len, lpad) = (get32(b, p), get32(b, p + 4), get32(b, p + 8));
        p += 12;
        let action = uleb(b, &mut p);
        rows.push((start, len, lpad, action));
    }
    rows
}

fn home(laid: &Laid<'_, '_>, name: &str) -> (u64, u64) {
    let p = laid.placed.iter().find(|p| p.symbol == sym(name)).unwrap();
    (p.at, p.len)
}

/// The checks shared by both platforms; returns the FDEs read back.
fn read_back(laid: &Laid<'_, '_>, eh: &EhSections<'_>) -> AVec<'static, FdeRead> {
    assert!(eh.eh_frame_at >= laid.text_end() && eh.end() <= laid.const_at, "the tables sit between the code and the const region");
    let fdes = read_eh_frame(&eh.eh_frame, eh.eh_frame_at);
    let (main_at, main_len) = home(laid, "main");
    let (thrower_at, thrower_len) = home(laid, "thrower");
    let (personality_at, _) = home(laid, "personality");
    assert_eq!(fdes.len(), 2, "one FDE per function with a frame (personality has none)");
    assert_eq!(eh.functions, 2);
    let main = fdes.iter().find(|f| f.pc_begin == main_at).expect("an FDE covers main");
    assert_eq!(main.pc_range, main_len);
    assert_eq!(main.personality, Some(personality_at), "main's CIE names the personality routine");
    let lsda = main.lsda.expect("main lands, so its FDE points to an LSDA");
    let except_end = eh.except_tab_at + eh.except_tab.len() as u64;
    assert!(lsda >= eh.except_tab_at && lsda < except_end, "the LSDA pointer points into except_tab");
    let rows = read_lsda(&eh.except_tab, (lsda - eh.except_tab_at) as usize);
    assert_eq!(&rows[..], &[(MAIN_ROW.start, MAIN_ROW.len, MAIN_ROW.landing, 0)], "the call site equals main's cleanup row");
    let thrower = fdes.iter().find(|f| f.pc_begin == thrower_at).expect("an FDE covers thrower");
    assert_eq!((thrower.pc_range, thrower.personality, thrower.lsda), (thrower_len, None, None));
    fdes
}

fn laid_tables(platform: Platform, check_index: impl Fn(&Laid<'_, '_>, &EhSections<'_>, &[FdeRead])) {
    let records = records();
    let functions = functions_of(&records);
    let members: [Member<'_>; 3] = functions.map(|f| Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    let plan = match platform {
        Platform::MacosArm64 => macho::plan(),
        Platform::LinuxArm64 => elf::plan(),
        Platform::Wasm64 => unreachable!("eh_out tests cover native arm64 platforms"),
    };
    let personality = Some(sym("personality"));
    let room = eh_room(&members, personality, platform, Alloc::global()).unwrap();
    let laid = lay_out(&members, &[], sym("main"), None, plan, room, Alloc::global()).unwrap();
    let eh = eh_sections(&laid, &members, personality, platform, Alloc::global()).unwrap();
    let fdes = read_back(&laid, &eh);
    check_index(&laid, &eh, &fdes[..]);
}

/// Prints `eh_out: the file's tables describe every row (Mach-O: unwind_info leads to them)`.
#[test]
fn eh_out_macho_tables_describe_every_row() {
    laid_tables(Platform::MacosArm64, |laid, eh, fdes| {
        let u = &eh.unwind_info;
        assert!(eh.eh_frame_hdr.is_empty());
        assert_eq!(get32(u, 0), 1, "unwind_info version 1");
        let index_at = get32(u, 20) as usize;
        let index_count = get32(u, 24) as usize;
        assert_eq!(index_count, 2, "one page and the end sentinel");
        let page = get32(u, index_at + 4) as usize;
        assert_eq!(get32(u, page), 2, "a regular second-level page");
        let count = u16::from_le_bytes([u[page + 6], u[page + 7]]) as usize;
        assert_eq!(count, 3, "an entry for every placed function, framed or not");
        let mut seen = 0;
        for k in 0..count {
            let entry = page + 8 + 8 * k;
            let (at, encoding) = (u64::from(get32(u, entry)), get32(u, entry + 4));
            let (main_at, _) = home(laid, "main");
            let (thrower_at, _) = home(laid, "thrower");
            if at == main_at {
                let fde = fdes.iter().find(|f| f.pc_begin == main_at).unwrap();
                assert_eq!(encoding, COMPACT_DWARF | fde.offset, "main has an LSDA: DWARF mode, naming its FDE");
                seen += 1;
            } else if at == thrower_at {
                assert_eq!(encoding, COMPACT_FRAME, "thrower's plain frame is compact frame mode");
                seen += 1;
            } else {
                assert_eq!(encoding, 0, "personality has no frame description");
            }
        }
        assert_eq!(seen, 2);
        let last = laid.placed.iter().filter(|p| !p.folded).map(|p| p.at + p.len).max().unwrap();
        assert_eq!(u64::from(get32(u, index_at + 12)), last, "the sentinel ends at the last function's end");
    });
    println!("eh_out: the file's tables describe every row (Mach-O: unwind_info leads to them)");
}

/// Prints `eh_out: the file's tables describe every row (ELF: eh_frame_hdr leads to them)`.
#[test]
fn eh_out_elf_tables_describe_every_row() {
    laid_tables(Platform::LinuxArm64, |_laid, eh, fdes| {
        let h = &eh.eh_frame_hdr;
        let hdr = eh.eh_frame_hdr_at;
        assert!(eh.unwind_info.is_empty());
        assert_eq!(&h[..4], &[1, 0x1b, 0x03, 0x3b], "version 1, pcrel|sdata4, udata4, datarel|sdata4");
        assert_eq!(pcrel(h, 4, hdr), eh.eh_frame_at, "eh_frame_ptr points at eh_frame");
        let count = get32(h, 8) as usize;
        assert_eq!(count, fdes.len());
        let mut last = 0u64;
        for k in 0..count {
            let loc = (hdr as i64 + i64::from(get32(h, 12 + 8 * k) as i32)) as u64;
            let fde = (hdr as i64 + i64::from(get32(h, 16 + 8 * k) as i32)) as u64;
            assert!(loc >= last, "the table is sorted by initial location");
            last = loc;
            let read = fdes.iter().find(|f| eh.eh_frame_at + u64::from(f.offset) == fde).expect("each entry names an FDE");
            assert_eq!(read.pc_begin, loc, "and that FDE begins at the entry's location");
        }
    });
    println!("eh_out: the file's tables describe every row (ELF: eh_frame_hdr leads to them)");
}

/// Prints `eh_out: the room is the measure plus the start's alignment; a byte short is refused`.
/// The real tables start [`TABLES_ALIGN`]-aligned after the code as the measure's do, so their
/// length from that start equals the measure (the 4-alignments inside both fall alike) and the
/// room `lay_out` keeps is that plus `ROOM_SLACK`, the start's alignment alone. The twin: the
/// same layout with its kept room one byte short of what the tables take is refused by type
/// (`EhRoom`), though the page the const region starts on still has room for them (so the check
/// reads the measure, not the page it was rounded to). The control: the room exactly what they
/// take is accepted.
#[test]
fn eh_out_the_room_is_the_measure_and_a_byte_short_is_refused_by_type() {
    let records = records();
    let functions = functions_of(&records);
    let members: [Member<'_>; 3] = functions.map(|f| Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    let personality = Some(sym("personality"));
    for platform in [Platform::MacosArm64, Platform::LinuxArm64] {
        let plan = match platform {
            Platform::MacosArm64 => macho::plan(),
            Platform::LinuxArm64 => elf::plan(),
            Platform::Wasm64 => unreachable!("eh_out tests cover native arm64 platforms"),
        };
        let room = eh_room(&members, personality, platform, Alloc::global()).unwrap();
        let mut laid = lay_out(&members, &[], sym("main"), None, plan, room, Alloc::global()).unwrap();
        assert_eq!(laid.tables_room, room, "the layout keeps what the measure said");
        let eh = eh_sections(&laid, &members, personality, platform, Alloc::global()).unwrap();
        let start = laid.text_end().next_multiple_of(TABLES_ALIGN);
        assert_eq!(eh.eh_frame_at, start, "the first table starts aligned after the code");
        assert_eq!(eh.end() - start, room - ROOM_SLACK, "every function placed: the tables are exactly the measure from their start");
        let need = eh.end() - laid.text_end();
        assert!(need <= room && laid.const_at - laid.text_end() >= need, "the page the const region starts on holds them");

        laid.tables_room = need - 1;
        let refused = eh_sections(&laid, &members, personality, platform, Alloc::global());
        assert!(
            matches!(refused, Err(ExportError::EhRoom { need: n, room: r }) if n == need && r == need - 1),
            "a room one byte short of the tables is refused by type, whatever the page leaves"
        );

        laid.tables_room = need;
        assert!(eh_sections(&laid, &members, personality, platform, Alloc::global()).is_ok(), "the control: exactly enough is accepted");
    }
    println!("eh_out: the room is the measure plus the start's alignment; a byte short is refused");
}

/// Prints `eh_out: refused: export: function <main> ...` (a row that lands with no personality
/// routine named is refused by name, never written without one).
#[test]
fn eh_out_a_landing_row_without_personality_is_refused() {
    let records = records();
    let functions = functions_of(&records);
    let mut out = Vec::new_in(Alloc::global());
    let Err(error) = export(&program(&functions, None), Platform::MacosArm64, &mut out, Alloc::global()) else {
        panic!("a landing row was exported with no personality routine");
    };
    assert_eq!(error, ExportError::NoPersonality { symbol: sym("main") });
    println!("eh_out: refused: {error}");
}

/// Prints `eh_out: exported on both platforms; panics: unwind tables for 2 functions`.
#[test]
fn eh_out_export_reports_and_checks() {
    let records = records();
    let functions = functions_of(&records);
    for platform in [Platform::MacosArm64, Platform::LinuxArm64] {
        let mut out = Vec::new_in(Alloc::global());
        let exported = export(&program(&functions, Some(sym("personality"))), platform, &mut out, Alloc::global()).unwrap();
        assert_eq!(exported.panics, Panics::Unwind { functions: 2 });
        let checked = match platform {
            Platform::MacosArm64 => check::macho(&out).map(|_| ()),
            Platform::LinuxArm64 => check::elf(&out).map(|_| ()),
            Platform::Wasm64 => unreachable!("eh_out tests cover native arm64 platforms"),
        };
        assert_eq!(checked, Ok(()), "the file passes the crate's own format checker");
    }
    println!("eh_out: exported on both platforms; panics: unwind tables for 2 functions");
}
