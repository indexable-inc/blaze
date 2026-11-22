//! The unwind rows round-trip and every cut or broken row is refused by name; the bytes
//! [`eh::entry`] makes for three hand-built functions are read back by a reader written here
//! from the documented forms (DWARF 5 section 6.4, "Call Frame Information", with the LSB's
//! `.eh_frame` changes; the Itanium C++ ABI's exception tables), whose LSDA steps are those of
//! the pinned library's `find_eh_action` (library/std/src/sys/personality/dwarf/eh.rs,
//! nightly-2026-10-27), quoted in eh.rs. A planted bug, a pad one instruction late, is caught.

use std::vec::Vec as StdVec;

use blaze_own::{AVec, Alloc, Vec};

use crate::eh::{self, EhError, Placed};
use crate::record::{CodeRecord, CodeView, Pad};
use crate::unwind::{self, Action, Frame, Row, UnwindError};

const NOP: u32 = 0xd503_201f;
const ENTRY_AT: u64 = 0x7000_0000;
const CODE_AT: u64 = 0x1_0000_4000;
const PERSONALITY: u64 = 0x1_0000_0100;

fn encoded(insts: u32, frame: Frame, rows: &[Row], alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for _ in 0..insts {
        r.code.extend_from_slice(&NOP.to_le_bytes());
    }
    unwind::fill(&mut r, frame, rows).unwrap();
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// A record's bytes with its rows written as raw pads, unchecked (to plant broken rows).
fn raw(insts: u32, word: u32, pads: &[Pad], alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for _ in 0..insts {
        r.code.extend_from_slice(&NOP.to_le_bytes());
    }
    r.frame = word;
    for &p in pads {
        r.pads.push(p);
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

fn row(start: u32, landing: u32, action: Action) -> Row {
    Row { start, len: 4, landing, action }
}

/// Prints `unwind rows: frame and 3 rows back as written; 3 broken words and 8 broken rows refused`.
#[test]
fn rows_round_trip_and_every_break_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let frame = Frame { x_pairs: 0b10001, d_pairs: 0b0010 };
    let rows = [row(40, 0, Action::Continue), row(24, 60, Action::Cleanup), row(48, 64, Action::Catch)];
    let bytes = encoded(24, frame, &rows, alloc);
    let view = CodeView::read(&bytes).unwrap();
    let read = unwind::read(view).unwrap();
    assert_eq!(read.frame(), Some(frame));
    let back: StdVec<Row> = (0..read.rows()).map(|i| read.row(i)).collect();
    // Sealed: sorted by start.
    assert_eq!(back, [rows[1], rows[0], rows[2]]);
    assert_eq!(Frame::of_word(frame.word().unwrap()).unwrap(), Some(frame));

    assert_eq!(Frame::of_word(0x2000_0000), Err(UnwindError::FrameForm { word: 0x2000_0000, form: 2 }));
    assert_eq!(Frame::of_word(0x1000_0020), Err(UnwindError::FrameBits { word: 0x1000_0020 }));
    assert!(matches!(Frame { x_pairs: 0x20, d_pairs: 0 }.word(), Err(UnwindError::FrameBits { .. })));

    let word = Frame::PLAIN.word().unwrap();
    let pad = |start, len, landing, action| Pad { start, len, landing, action };
    let cases: [(u32, StdVec<Pad>, UnwindError); 8] = [
        (0, vec![pad(8, 4, 0, 0)], UnwindError::PadsWithoutFrame { pads: 1 }),
        (word, vec![pad(8, 4, 16, 9)], UnwindError::PadAction { start: 8, action: 9 }),
        (word, vec![pad(8, 0, 0, 0)], UnwindError::PadShape { start: 8, len: 0 }),
        (word, vec![pad(10, 4, 0, 0)], UnwindError::PadShape { start: 10, len: 4 }),
        (word, vec![pad(4, 4, 0, 0)], UnwindError::PadInPrologue { start: 4, prologue: 8 }),
        (word, vec![pad(8, 8, 0, 0), pad(12, 4, 0, 0)], UnwindError::PadsOverlap { start: 12 }),
        (word, vec![pad(8, 4, 0, 1)], UnwindError::PadLanding { start: 8, landing: 0, action: Action::Cleanup }),
        (word, vec![pad(8, 4, 16, 0)], UnwindError::PadLanding { start: 8, landing: 16, action: Action::Continue }),
    ];
    for (word, pads, want) in &cases {
        let bytes = raw(8, *word, pads, alloc);
        let got = unwind::read(CodeView::read(&bytes).unwrap()).err();
        assert_eq!(got, Some(*want), "rows {pads:?}");
    }
    println!("unwind rows: frame and 3 rows back as written; 3 broken words and {} broken rows refused", cases.len());
}

/// Prints `unwind rows: every one of N cuts of an encoded record refused`.
#[test]
fn every_cut_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = encoded(16, Frame::PLAIN, &[row(12, 40, Action::Cleanup), row(20, 0, Action::Continue)], alloc);
    for cut in 0..bytes.len() {
        assert!(CodeView::read(&bytes[..cut]).is_err(), "a record cut at {cut} of {} bytes was read", bytes.len());
    }
    println!("unwind rows: every one of {} cuts of an encoded record refused", bytes.len());
}

// The reader: DWARF 5 section 6.4 and the LSB's `.eh_frame`, then `find_eh_action`'s steps.

struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> u8 {
        self.at += 1;
        self.b[self.at - 1]
    }
    fn u32(&mut self) -> u32 {
        self.at += 4;
        u32::from_le_bytes(self.b[self.at - 4..self.at].try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        self.at += 8;
        u64::from_le_bytes(self.b[self.at - 8..self.at].try_into().unwrap())
    }
    fn uleb(&mut self) -> u64 {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let byte = self.u8();
            v |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return v;
            }
        }
    }
    fn sleb(&mut self) -> i64 {
        let (mut v, mut shift) = (0i64, 0);
        loop {
            let byte = self.u8();
            v |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    v |= -1i64 << shift;
                }
                return v;
            }
        }
    }
}

/// What the reader found for one pc: the frame address rule, the saved registers' offsets from
/// it, and the FDE's facts.
#[derive(Debug)]
struct Unwound {
    cfa: (u64, i64),
    saved: StdVec<(u64, i64)>,
    pc_begin: u64,
    pc_range: u64,
    personality: Option<u64>,
    lsda: Option<u64>,
}

/// Reads the run of entries in `b` (at `ENTRY_AT`): one CIE, one FDE, then a zero length; runs
/// the CIE's and the FDE's instructions up to `pc`.
fn unwind_at(b: &[u8], pc: u64) -> Unwound {
    let mut r = Bytes { b, at: 0 };
    let cie_len = r.u32() as usize;
    let cie_end = 4 + cie_len;
    assert_eq!(r.u32(), 0, "CIE id");
    assert_eq!(r.u8(), 1, "CIE version");
    let mut aug = StdVec::new();
    loop {
        match r.u8() {
            0 => break,
            c => aug.push(c),
        }
    }
    let code_align = r.uleb();
    let data_align = r.sleb();
    assert_eq!(r.uleb(), 30, "return address register");
    assert_eq!(aug[0], b'z');
    let aug_len = r.uleb() as usize;
    let aug_end = r.at + aug_len;
    let mut personality = None;
    let mut has_lsda = false;
    for &c in &aug[1..] {
        match c {
            b'P' => {
                assert_eq!(r.u8(), eh::ABSPTR);
                personality = Some(r.u64());
            }
            b'L' => {
                assert_eq!(r.u8(), eh::ABSPTR);
                has_lsda = true;
            }
            b'R' => assert_eq!(r.u8(), eh::ABSPTR),
            other => panic!("augmentation letter {}", other as char),
        }
    }
    assert_eq!(r.at, aug_end);
    let cie_program = (r.at, cie_end);
    r.at = cie_end;
    let fde = r.at;
    let fde_len = r.u32() as usize;
    let fde_end = fde + 4 + fde_len;
    let back = r.u32() as usize;
    assert_eq!(fde + 4 - back, 0, "the FDE's CIE pointer reaches its CIE");
    let pc_begin = r.u64();
    let pc_range = r.u64();
    let aug_len = r.uleb() as usize;
    let lsda = match has_lsda {
        true => {
            assert_eq!(aug_len, 8);
            Some(r.u64())
        }
        false => {
            assert_eq!(aug_len, 0);
            None
        }
    };
    let fde_program = (r.at, fde_end);
    assert_eq!(u32::from_le_bytes(b[fde_end..fde_end + 4].try_into().unwrap()), 0, "the run ends");
    let mut cfa = (0u64, 0i64);
    let mut saved = StdVec::new();
    let mut loc = pc_begin;
    'run: for (start, end) in [cie_program, fde_program] {
        let mut p = Bytes { b, at: start };
        while p.at < end {
            let op = p.u8();
            match (op & 0xc0, op) {
                (0x40, _) => {
                    loc += u64::from(op & 0x3f) * code_align;
                    if loc > pc {
                        break 'run;
                    }
                }
                (0x80, _) => {
                    let off = p.uleb() as i64 * data_align;
                    saved.retain(|s: &(u64, i64)| s.0 != u64::from(op & 0x3f));
                    saved.push((u64::from(op & 0x3f), off));
                }
                (_, eh::CFA_OFFSET_EXTENDED) => {
                    let reg = p.uleb();
                    let off = p.uleb() as i64 * data_align;
                    saved.retain(|s: &(u64, i64)| s.0 != reg);
                    saved.push((reg, off));
                }
                (_, eh::CFA_DEF_CFA) => cfa = (p.uleb(), p.uleb() as i64),
                (_, eh::CFA_DEF_CFA_REGISTER) => cfa.0 = p.uleb(),
                (_, eh::CFA_DEF_CFA_OFFSET) => cfa.1 = p.uleb() as i64,
                (_, eh::CFA_NOP) => {}
                _ => panic!("call frame instruction {op:#x}"),
            }
        }
    }
    saved.sort();
    Unwound { cfa, saved, pc_begin, pc_range, personality, lsda }
}

/// `EHAction`, as `find_eh_action` gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Found {
    None,
    Cleanup(u64),
    Catch(u64),
    Filter(u64),
    Terminate,
}

/// `find_eh_action` over the LSDA at address `lsda` in `b` (at `ENTRY_AT`), for a call whose
/// return address is `ret`: `ip` is `ret - 1` ("The return address points 1 byte past the call
/// instruction").
fn find_action(b: &[u8], lsda: u64, func_start: u64, ret: u64) -> Found {
    let ip = ret - 1;
    let mut r = Bytes { b, at: (lsda - ENTRY_AT) as usize };
    let start_encoding = r.u8();
    assert_eq!(start_encoding, eh::OMIT);
    let lpad_base = func_start;
    let ttype_encoding = r.u8();
    if ttype_encoding != eh::OMIT {
        r.uleb();
    }
    let call_site_encoding = r.u8();
    assert_eq!(call_site_encoding, eh::UDATA4);
    let call_site_table_length = r.uleb() as usize;
    let action_table = r.at + call_site_table_length;
    while r.at < action_table {
        let cs_start = u64::from(r.u32());
        let cs_len = u64::from(r.u32());
        let cs_lpad = u64::from(r.u32());
        let cs_action_entry = r.uleb();
        if ip < func_start + cs_start {
            break;
        }
        if ip < func_start + cs_start + cs_len {
            if cs_lpad == 0 {
                return Found::None;
            }
            let lpad = lpad_base + cs_lpad;
            if cs_action_entry == 0 {
                return Found::Cleanup(lpad);
            }
            let mut a = Bytes { b, at: action_table + cs_action_entry as usize - 1 };
            let ttype_index = a.sleb();
            let next_action = a.sleb();
            return match (next_action, ttype_index) {
                (n, _) if n != 0 => Found::Catch(lpad),
                (_, 0) => Found::Cleanup(lpad),
                (_, t) if t > 0 => Found::Catch(lpad),
                _ => Found::Filter(lpad),
            };
        }
    }
    Found::Terminate
}

/// What each row's call should find: `Found` for an unwind out of the call at `start`.
fn expected(row: Row) -> Found {
    let lpad = CODE_AT + u64::from(row.landing);
    match row.action {
        Action::Continue => Found::None,
        Action::Cleanup => Found::Cleanup(lpad),
        Action::Catch => Found::Catch(lpad),
        Action::Terminate => Found::Filter(lpad),
    }
}

/// Builds `rows`' entry (its pads written as `planted` says) and checks every call: each row's
/// call (its return address the instruction after it) finds its action; a call at `unlisted`
/// finds `Terminate`. Gives the first mismatch.
fn check(insts: u32, frame: Frame, rows: &[Row], planted: &[Row], unlisted: &[u32]) -> Result<StdVec<u8>, String> {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = encoded(insts, frame, planted, alloc);
    let read = unwind::read(CodeView::read(&bytes).unwrap()).unwrap();
    let mut out = Vec::new_in(alloc);
    let placed = Placed { entry_at: ENTRY_AT, code_at: CODE_AT, personality: Some(PERSONALITY) };
    let made = eh::entry(&read, placed, &mut out).unwrap();
    let b = out.to_vec();
    let body = unwind_at(&b, CODE_AT + u64::from(frame.prologue()));
    assert_eq!((body.pc_begin, body.pc_range), (CODE_AT, u64::from(insts) * 4));
    assert_eq!(body.lsda, made.lsda.map(|at| ENTRY_AT + at as u64));
    for r in rows {
        let found = match body.lsda {
            Some(lsda) => find_action(&b, lsda, body.pc_begin, CODE_AT + u64::from(r.start + r.len)),
            None => Found::None,
        };
        if found != expected(*r) {
            return Err(format!("the call at {} finds {found:?}, not {:?}", r.start, expected(*r)));
        }
    }
    for &at in unlisted {
        let found = find_action(&b, body.lsda.unwrap(), body.pc_begin, CODE_AT + u64::from(at) + 4);
        if found != Found::Terminate {
            return Err(format!("the unlisted call at {at} finds {found:?}, not Terminate"));
        }
    }
    Ok(b)
}

/// Prints `eh: 3 functions' CIE, FDE and LSDA read back as the personality reads them`.
#[test]
fn three_functions_read_back() {
    let alloc = blaze_own::testing::heap().alloc();
    // One: a frame and no pads: `zR`, no personality, no LSDA.
    let bytes = encoded(8, Frame::PLAIN, &[], alloc);
    let read = unwind::read(CodeView::read(&bytes).unwrap()).unwrap();
    let mut out = Vec::new_in(alloc);
    let placed = Placed { entry_at: ENTRY_AT, code_at: CODE_AT, personality: None };
    let made = eh::entry(&read, placed, &mut out).unwrap();
    assert_eq!(made.lsda, None);
    let at0 = unwind_at(&out, CODE_AT);
    assert_eq!((at0.cfa, at0.saved.len(), at0.personality, at0.lsda), ((31, 0), 0, None, None));
    let at1 = unwind_at(&out, CODE_AT + 4);
    assert_eq!((at1.cfa, &at1.saved[..]), ((31, 16), &[(29, -16), (30, -8)][..]));
    let body = unwind_at(&out, CODE_AT + 20);
    assert_eq!((body.cfa, &body.saved[..]), ((29, 16), &[(29, -16), (30, -8)][..]));

    // Two: a cleanup and a call that unwinds on; the call at 32 has no row.
    let rows = [row(12, 0, Action::Continue), row(20, 40, Action::Cleanup)];
    let b = check(16, Frame::PLAIN, &rows, &rows, &[32]).unwrap();
    assert_eq!(unwind_at(&b, CODE_AT + 8).personality, Some(PERSONALITY));

    // Three: saved pairs, a catch, a terminate and a cleanup.
    let frame = Frame { x_pairs: 0b101, d_pairs: 0b1 };
    let rows = [row(24, 60, Action::Catch), row(36, 72, Action::Terminate), row(48, 64, Action::Cleanup)];
    let b = check(20, frame, &rows, &rows, &[52]).unwrap();
    let body = unwind_at(&b, CODE_AT + u64::from(frame.prologue()));
    let want = [(19, -24), (20, -32), (23, -40), (24, -48), (29, -16), (30, -8), (72, -56), (73, -64)];
    assert_eq!((body.cfa, &body.saved[..]), ((29, 16), &want[..]));

    // No frame, no entry; a landing with no routine, refused.
    let bytes = raw(4, 0, &[], alloc);
    let read = unwind::read(CodeView::read(&bytes).unwrap()).unwrap();
    assert_eq!(eh::entry(&read, placed, &mut out), Err(EhError::NoFrame));
    let bytes = encoded(16, Frame::PLAIN, &[row(20, 40, Action::Cleanup)], alloc);
    let read = unwind::read(CodeView::read(&bytes).unwrap()).unwrap();
    assert_eq!(eh::entry(&read, placed, &mut out), Err(EhError::NoPersonality));
    println!("eh: 3 functions' CIE, FDE and LSDA read back as the personality reads them");
}

/// Prints `eh: a pad one instruction late is caught: the call at 20 finds Terminate, not Cleanup(..)`.
#[test]
fn a_pad_one_instruction_late_is_caught() {
    let rows = [row(20, 40, Action::Cleanup)];
    let late = [Row { start: 24, ..rows[0] }];
    check(16, Frame::PLAIN, &rows, &rows, &[]).unwrap();
    let caught = check(16, Frame::PLAIN, &rows, &late, &[]).unwrap_err();
    assert!(caught.contains("finds Terminate"), "{caught}");
    println!("eh: a pad one instruction late is caught: {caught}");
}
