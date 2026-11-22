//! jit_debug.rs's tests: the debugger object read back (its symbols and its line rows, file by
//! file), a pc's inline chain naming each spliced body, and the descriptor's list as a debugger
//! would walk it.

use blaze_own::{AVec, Alloc, Vec};
use blaze_run::InstanceKey;
use blaze_tier::{Arch, InlineFrame, OUTERMOST, Position};

use super::{Code, JitError, Jit, JitError::Taken, Line, Lines, Located, chain, descriptor_now, object};

const A: InstanceKey = InstanceKey([0xa; 32]);
const B: InstanceKey = InstanceKey([0xb; 32]);
const C: InstanceKey = InstanceKey([0xc; 32]);
const UNKNOWN: InstanceKey = InstanceKey([0xd; 32]);

/// Item texts: A's in file 7 ("src/a.rs"), B's and C's in file 3 ("src/b.rs"); each item
/// starts at line 10, and offset `o` is line `10 + o / 10`, column `o % 10 + 1`.
struct Texts;

impl Lines for Texts {
    fn locate(&self, instance: InstanceKey, at: u32) -> Option<Line> {
        let file = match instance {
            A => 7,
            B | C => 3,
            _ => return None,
        };
        Some(Line { file, line: 10 + at / 10, column: at % 10 + 1 })
    }

    fn file(&self, file: u32) -> Option<&[u8]> {
        match file {
            7 => Some(b"src/a.rs"),
            3 => Some(b"src/b.rs"),
            _ => None,
        }
    }
}

fn pos(at: u32, frame: u32, span_lo: u32) -> Position {
    Position { at, frame, span_lo, span_hi: span_lo + 1 }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(word)
}

fn uleb_at(b: &[u8], at: &mut usize) -> u64 {
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

fn sleb_at(b: &[u8], at: &mut usize) -> i64 {
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

fn cstr(b: &[u8], at: usize) -> &[u8] {
    let len = b[at..].iter().position(|&c| c == 0).expect("a NUL");
    &b[at..at + len]
}

/// Section `name`'s (offset, size, addr, type).
fn section<'o>(obj: &'o [u8], name: &[u8]) -> (&'o [u8], u64, u32) {
    let shoff = u64_at(obj, 0x28) as usize;
    let count = u16_at(obj, 0x3c) as usize;
    let names = shoff + 64 * u16_at(obj, 0x3e) as usize;
    let names_at = u64_at(obj, names + 24) as usize;
    for i in 0..count {
        let header = shoff + 64 * i;
        if cstr(obj, names_at + u32_at(obj, header) as usize) == name {
            let (offset, size) = (u64_at(obj, header + 24) as usize, u64_at(obj, header + 32) as usize);
            let kind = u32_at(obj, header + 4);
            let bytes = if kind == 8 { &obj[0..0] } else { &obj[offset..offset + size] };
            return (bytes, u64_at(obj, header + 16), kind);
        }
    }
    panic!("no section {}", std::string::String::from_utf8_lossy(name));
}

/// One line row as a debugger reads it: address, file path, line, column; the end of a
/// sequence is a row with an empty path.
type Read<'o> = (u64, &'o [u8], i64, u64);

/// Runs the line program of the unit at `unit`, appending its rows; gives the next unit's offset.
fn run_unit<'o>(line: &'o [u8], unit: usize, out: &mut AVec<'_, Read<'o>>) -> usize {
    let end = unit + 4 + u32_at(line, unit) as usize;
    assert_eq!(u16_at(line, unit + 4), 4, "DWARF 4");
    let program = unit + 10 + u32_at(line, unit + 6) as usize;
    let mut at = unit + 10 + 6 + 12;
    assert_eq!(line[at], 0, "no include directories");
    at += 1;
    let mut files: AVec<'_, &[u8]> = Vec::new_in(Alloc::global());
    while line[at] != 0 {
        let name = cstr(line, at);
        files.push(name);
        at += name.len() + 1;
        for _ in 0..3 {
            uleb_at(line, &mut at);
        }
    }
    assert_eq!(at + 1, program, "the header length covers the file table");
    let (mut address, mut file, mut row, mut column) = (0u64, 1u64, 1i64, 0u64);
    at = program;
    while at < end {
        let op = line[at];
        at += 1;
        match op {
            0 => {
                let len = uleb_at(line, &mut at) as usize;
                match line[at] {
                    2 => address = u64_at(line, at + 1),
                    1 => out.push((address, b"", 0, 0)),
                    other => panic!("extended opcode {other}"),
                }
                at += len;
            }
            1 => out.push((address, files[file as usize - 1], row, column)),
            2 => address += uleb_at(line, &mut at),
            3 => row += sleb_at(line, &mut at),
            4 => file = uleb_at(line, &mut at),
            5 => column = uleb_at(line, &mut at),
            other => panic!("opcode {other}"),
        }
    }
    end
}

/// The object for a batch of two functions, read back as a debugger would: an ELF64 for arm64,
/// `.text` at the batch's addresses, a symbol each with its address and size, and per function
/// a line table whose rows are the innermost body's file, line and column at each position
/// row's address (a spliced B body's row is in src/b.rs), ending at the function's end. The
/// same batch gives the same bytes; a row naming an instance with no known text is refused by
/// name. Prints "jit_debug: the object names symbols and lines".
#[test]
fn the_object_names_symbols_and_lines() {
    let frames = [InlineFrame { parent: OUTERMOST, instance: B, call_lo: 12, call_hi: 15 }];
    let first = [pos(0, OUTERMOST, 0), pos(8, 0, 20), pos(16, OUTERMOST, 33)];
    let second = [pos(0, OUTERMOST, 0)];
    let batch = [
        Code { at: 0x1000, len: 32, instance: A, name: b"alpha", positions: &first, frames: &frames },
        Code { at: 0x1100, len: 8, instance: A, name: b"beta", positions: &second, frames: &[] },
    ];
    let obj = object(&batch, Arch::Arm64, &Texts, Alloc::global()).expect("the object");
    assert_eq!(&obj[..4], b"\x7fELF");
    assert_eq!((obj[4], obj[5], u16_at(&obj, 16), u16_at(&obj, 18)), (2, 1, 2, 183), "ELF64 little-endian ET_EXEC for AArch64");

    let (_, text_at, text_kind) = section(&obj, b".text");
    assert_eq!((text_at, text_kind), (0x1000, 8), "a NOBITS .text at the batch's first address");
    let (symtab, _, _) = section(&obj, b".symtab");
    let (strtab, _, _) = section(&obj, b".strtab");
    assert_eq!(symtab.len(), 3 * 24, "the null symbol and one a function");
    for (i, (name, at, len)) in [(&b"alpha"[..], 0x1000u64, 32u64), (&b"beta"[..], 0x1100, 8)].into_iter().enumerate() {
        let sym = &symtab[24 * (i + 1)..];
        assert_eq!(cstr(strtab, u32_at(sym, 0) as usize), name);
        assert_eq!((sym[4], u16_at(sym, 6), u64_at(sym, 8), u64_at(sym, 16)), (0x12, 1, at, len));
    }

    let (line, _, _) = section(&obj, b".debug_line");
    let mut rows: AVec<'_, Read<'_>> = Vec::new_in(Alloc::global());
    let next = run_unit(line, 0, &mut rows);
    run_unit(line, next, &mut rows);
    let want: [Read<'_>; 6] = [
        (0x1000, b"src/a.rs", 10, 1),
        (0x1008, b"src/b.rs", 12, 1),
        (0x1010, b"src/a.rs", 13, 4),
        (0x1020, b"", 0, 0),
        (0x1100, b"src/a.rs", 10, 1),
        (0x1108, b"", 0, 0),
    ];
    assert_eq!(rows.as_slice(), &want[..]);
    let (info, _, _) = section(&obj, b".debug_info");
    assert_eq!(cstr(info, 4 + 2 + 4 + 1 + 1), b"alpha", "the first unit is alpha's");

    let again = object(&batch, Arch::Arm64, &Texts, Alloc::global()).expect("again");
    assert_eq!(obj.as_slice(), again.as_slice(), "deterministic");

    let unknown = [Code { at: 0x1000, len: 4, instance: UNKNOWN, name: b"gamma", positions: &second, frames: &[] }];
    assert_eq!(object(&unknown, Arch::Arm64, &Texts, Alloc::global()).err(), Some(JitError::NoLine { instance: UNKNOWN, offset: 0 }));
    let overlapping = [batch[0], Code { at: 0x101c, ..batch[1] }];
    assert_eq!(object(&overlapping, Arch::Arm64, &Texts, Alloc::global()).err(), Some(JitError::Unsorted { at: 0x101c }));
    println!("jit_debug: the object names symbols and lines");
}

/// A pc in code C was spliced into (C into B at 40..44, B into the function A at 12..15) gives
/// the chain C's own span, then B at the call of C, then A at the call of B; a pc in A's own code
/// gives A alone; a frame spliced into itself (a planted cycle) is refused by name, never walked.
/// Prints "jit_debug: an inline chain names spliced bodies".
#[test]
fn an_inline_chain_names_spliced_bodies() {
    let frames = [
        InlineFrame { parent: OUTERMOST, instance: B, call_lo: 12, call_hi: 15 },
        InlineFrame { parent: 0, instance: C, call_lo: 40, call_hi: 44 },
    ];
    let positions = [Position { at: 0, frame: OUTERMOST, span_lo: 0, span_hi: 5 }, Position { at: 8, frame: 1, span_lo: 50, span_hi: 60 }];
    let code = Code { at: 0x4000, len: 16, instance: A, name: b"alpha", positions: &positions, frames: &frames };
    let mut out: AVec<'_, Located> = Vec::new_in(Alloc::global());
    chain(&code, 9, &mut out).expect("the chain");
    let want = [
        Located { instance: C, span_lo: 50, span_hi: 60, inlined: true },
        Located { instance: B, span_lo: 40, span_hi: 44, inlined: true },
        Located { instance: A, span_lo: 12, span_hi: 15, inlined: false },
    ];
    assert_eq!(out.as_slice(), &want[..]);
    out.clear();
    chain(&code, 3, &mut out).expect("the outer body");
    assert_eq!(out.as_slice(), &[Located { instance: A, span_lo: 0, span_hi: 5, inlined: false }][..]);

    let cycle = [InlineFrame { parent: 0, instance: B, call_lo: 1, call_hi: 2 }];
    let looped = Code { frames: &cycle, positions: &[Position { at: 0, frame: 0, span_lo: 0, span_hi: 1 }], ..code };
    out.clear();
    assert_eq!(chain(&looped, 0, &mut out), Err(JitError::ParentAfterChild { at: 0x4000, frame: 0, parent: 0 }));
    let late = Code { positions: &[Position { at: 4, frame: OUTERMOST, span_lo: 0, span_hi: 1 }], ..code };
    assert_eq!(chain(&late, 2, &mut out), Err(JitError::NoPosition { at: 0x4000, offset: 2 }));
    println!("jit_debug: an inline chain names spliced bodies");
}

/// Registering links the object at the list's head with `JIT_REGISTER_FN` (what a debugger
/// stopped in `__jit_debug_register_code` reads); a second batch goes in front; dropping a batch
/// unlinks it and leaves the action at `JIT_NOACTION`; a second claim is refused while one is
/// held. Prints "jit_debug: the descriptor lists what is registered".
#[test]
fn the_descriptor_lists_what_is_registered() {
    let jit = Jit::claim().expect("the claim");
    assert!(matches!(Jit::claim(), Err(Taken)));
    let mut one: AVec<'_, u8> = Vec::new_in(Alloc::global());
    one.extend_from_slice(b"one object");
    let one_at = one.as_ptr();
    let first = jit.register(one, Alloc::global());
    assert_eq!(descriptor_now(), (one_at, 10, super::JIT_REGISTER_FN));
    let mut two: AVec<'_, u8> = Vec::new_in(Alloc::global());
    two.extend_from_slice(b"two");
    let two_at = two.as_ptr();
    let second = jit.register(two, Alloc::global());
    assert_eq!(descriptor_now(), (two_at, 3, super::JIT_REGISTER_FN));
    drop(second);
    assert_eq!(descriptor_now(), (one_at, 10, super::JIT_NOACTION));
    drop(first);
    assert_eq!(descriptor_now(), (core::ptr::null(), 0, super::JIT_NOACTION));
    drop(jit);
    assert!(Jit::claim().is_ok(), "the claim is given back on drop");
    println!("jit_debug: the descriptor lists what is registered");
}
