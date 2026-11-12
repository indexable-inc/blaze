//! A record's bytes: deterministic CBOR that reads back as the record it was, and refuses
//! what is not one; and a record placed: each kind of fixup against each kind of place.

use blaze_protocol::cbor;
use hegel::TestCase;
use hegel::generators as gs;

use crate::KEPT;
use crate::place::{Bound, place, place_static, pool};
use crate::record::{Datum, Fixup, Kind, LAYOUT, Place, Record, To, Unwind, What};
use crate::wire::{Arch, List, WireError};

fn pick(tc: &TestCase, max: usize) -> usize {
    tc.draw(gs::integers::<usize>().max_value(max))
}

/// A record with what it borrows owned, so a drawn one outlives its draw.
#[derive(Clone, Debug)]
struct Owned {
    arm: bool,
    what: What,
    align: u32,
    bytes: Vec<u8>,
    zeroed: Option<u64>,
    fixups: Vec<Fixup>,
    places: Vec<(u8, String)>,
    data: Vec<(u8, u32, Vec<u8>, bool)>,
    data_fixups: Vec<Fixup>,
    unwind: Option<(u32, i32, u16, Vec<u8>, Vec<u8>, bool, bool)>,
    name: String,
}

struct Lists<'a> {
    places: Vec<Place<'a>>,
    data: Vec<Datum<'a>>,
}

impl Owned {
    fn lists(&self) -> Lists<'_> {
        let places = self
            .places
            .iter()
            .map(|(kind, name)| match kind {
                0 => Place::Function,
                1 => Place::Static,
                2 => Place::Extern(name),
                _ => Place::Weak(name),
            })
            .collect();
        let data = self
            .data
            .iter()
            .enumerate()
            .map(|(index, (seed, align, bytes, writable))| Datum {
                id: [*seed; 32],
                align: *align,
                bytes,
                // The first datum has no fixup: one may only name a datum before it.
                fixups: List::of(if index == 0 { &[] } else { &self.data_fixups }),
                writable: *writable,
            })
            .collect();
        Lists { places, data }
    }

    fn record<'a>(&'a self, lists: &'a Lists<'a>) -> Record<'a> {
        let first_function = self.places.iter().position(|it| it.0 == 0);
        Record {
            arch: if self.arm { Arch::Aarch64 } else { Arch::X86_64 },
            what: self.what,
            align: self.align,
            size: self.zeroed.unwrap_or(self.bytes.len() as u64),
            bytes: if self.zeroed.is_some() { &[] } else { &self.bytes },
            fixups: List::of(if self.zeroed.is_some() { &[] } else { &self.fixups }),
            places: List::of(&lists.places),
            data: List::of(&lists.data),
            unwind: self.unwind.as_ref().filter(|_| self.what == What::Code).map(
                |(code_align, data_align, return_address, cie, fde, personality, lsda)| Unwind {
                    code_align: *code_align,
                    data_align: *data_align,
                    return_address: *return_address,
                    cie,
                    fde,
                    personality: first_function.filter(|_| *personality).map(|it| it as u32),
                    lsda: (*lsda && !self.data.is_empty()).then_some(0),
                },
            ),
            name: &self.name,
        }
    }
}

fn owned(tc: &TestCase) -> Owned {
    let what = match pick(tc, 4) {
        0..=2 => What::Code,
        _ => What::Static { mutable: tc.draw(gs::booleans()), thread_local: tc.draw(gs::booleans()) },
    };
    let bytes: Vec<u8> = (0..8 + pick(tc, 300)).map(|at| at as u8).collect();
    let places: Vec<(u8, String)> = (0..pick(tc, 6))
        .map(|at| (pick(tc, 3) as u8, ["write", "_NSGetArgv", "λ"][at % 3].repeat(1 + pick(tc, 2))))
        .collect();
    let data: Vec<(u8, u32, Vec<u8>, bool)> = (0..pick(tc, 4))
        .map(|at| (at as u8, 1 << pick(tc, 6), vec![at as u8; 8 + pick(tc, 40)], tc.draw(gs::booleans())))
        .collect();
    let to = |tc: &TestCase, data_before: usize, own: bool| match pick(tc, 2) {
        0 if !places.is_empty() => To::Place(pick(tc, places.len() - 1) as u32),
        1 if data_before > 0 => To::Data(pick(tc, data_before - 1) as u32),
        _ if own => To::Own,
        _ if !places.is_empty() => To::Place(0),
        _ => To::Data(0),
    };
    let addend = |tc: &TestCase| [0, -4, 16, i64::MIN, i64::MAX][pick(tc, 4)];
    let fixups = (0..pick(tc, 12))
        .map(|_| Fixup {
            at: pick(tc, bytes.len() - 8) as u32,
            kind: Kind::ALL[pick(tc, Kind::ALL.len() - 1)],
            to: to(tc, data.len(), true),
            addend: addend(tc),
        })
        .collect();
    // Shared by every datum after the first: against a place, or the first datum.
    let data_fixups = match places.is_empty() && data.len() < 2 {
        true => Vec::new(),
        false => (0..pick(tc, 3))
            .map(|_| Fixup { at: 0, kind: Kind::Abs64, to: to(tc, 1, false), addend: addend(tc) })
            .collect(),
    };
    Owned {
        arm: tc.draw(gs::booleans()),
        what,
        align: 1 << pick(tc, 12),
        bytes,
        zeroed: (what != What::Code && tc.draw(gs::booleans())).then(|| pick(tc, 1 << 20) as u64),
        fixups,
        places,
        data,
        data_fixups,
        unwind: tc.draw(gs::booleans()).then(|| {
            (
                pick(tc, 4) as u32,
                -(pick(tc, 8) as i32),
                pick(tc, 30) as u16,
                vec![0x0c; pick(tc, 8)],
                vec![0x0e; pick(tc, 40)],
                tc.draw(gs::booleans()),
                tc.draw(gs::booleans()),
            )
        }),
        name: ["", "hello::main", "<T as core::fmt::Debug>::fmt"][pick(tc, 2)].to_owned(),
    }
}

/// The invariant: a record is written as one item in the core deterministic encoding (so one
/// record has one encoding, and its hash is its id), and reads back as the record it was;
/// any cut of it is refused, never read as a shorter record.
#[hegel::test(test_cases = 400)]
fn every_record_is_deterministic_cbor_and_reads_back(tc: TestCase) {
    let owned = owned(&tc);
    let lists = owned.lists();
    let record = owned.record(&lists);
    let mut bytes = Vec::new();
    record.encode(&mut bytes);
    if let Err(why) = cbor::check(&bytes) {
        panic!("{owned:?} is not deterministic CBOR: {why}");
    }
    assert_eq!(Record::decode(&bytes), Ok(record), "{owned:?}");
    let mut again = Vec::new();
    Record::decode(&bytes).expect("read").encode(&mut again);
    assert_eq!(again, bytes, "a record read and written again is other bytes");
    let cut = pick(&tc, bytes.len() - 1);
    assert!(Record::decode(&bytes[..cut]).is_err(), "{owned:?} cut at {cut} was read");
}

fn code<'a>(
    arch: Arch,
    bytes: &'a [u8],
    fixups: &'a [Fixup],
    places: &'a [Place<'a>],
    data: &'a [Datum<'a>],
) -> Record<'a> {
    Record {
        arch,
        what: What::Code,
        align: 4,
        size: bytes.len() as u64,
        bytes,
        fixups: List::of(fixups),
        places: List::of(places),
        data: List::of(data),
        unwind: None,
        name: "f",
    }
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks(4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]])).collect()
}

fn le(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|it| it.to_le_bytes()).collect()
}

/// The control of the invariant above: what differs from a record only in what its layout
/// forbids is refused by name, each with the reader's own check.
#[test]
fn the_oracle_catches_a_record_of_another_layout_and_an_index_past_its_list() {
    let fixups = [Fixup { at: 0, kind: Kind::Call26, to: To::Place(0), addend: 0 }];
    let bytes = le(&[0x9400_0000]);
    let good = code(Arch::Aarch64, &bytes, &fixups, &[Place::Function], &[]);
    let mut out = Vec::new();
    good.encode(&mut out);
    assert_eq!(Record::decode(&out), Ok(good));
    // Another layout: the first field.
    assert_eq!(LAYOUT, 1);
    let mut other = out.clone();
    other[1] = 0x02;
    assert_eq!(Record::decode(&other), Err(WireError::Schema { said: 2 }));
    // A fixup against a place the record has not.
    let mut out = Vec::new();
    code(Arch::Aarch64, &bytes, &fixups, &[], &[]).encode(&mut out);
    assert!(matches!(Record::decode(&out), Err(WireError::Range { .. })));
    // A fixup whose field ends past the bytes.
    let past = [Fixup { at: 2, kind: Kind::Call26, to: To::Own, addend: 0 }];
    let mut out = Vec::new();
    code(Arch::Aarch64, &bytes, &past, &[], &[]).encode(&mut out);
    assert!(matches!(Record::decode(&out), Err(WireError::Range { .. })));
}

/// The invariant (launcher.md, "Step 3"; the table in `record`'s documentation): placed on
/// AArch64, a GOT fixup against a function names its slot word, any other its entry thunk;
/// a GOT fixup against an outside symbol names a pool cell that holds the symbol's address;
/// a datum and the code itself are named directly; and the pool follows the code, 8-aligned.
#[test]
fn an_aarch64_record_is_placed_against_words_thunks_and_its_own_pool() {
    const AT: u64 = 0x1_0000_4000;
    const WORD: u64 = 0x1_0000_0018;
    const THUNK: u64 = 0x1_0000_2030;
    const EXTERN: u64 = 0x7fff_1234_5678;
    const DATUM: u64 = 0x2_0000_0100;
    let bytes = le(&[
        0x9000_0010, //  0: adrp x16, word
        0xf940_0210, //  4: ldr  x16, [x16, word]
        0xd63f_0200, //  8: blr  x16
        0x9000_0010, // 12: adrp x16, thunk
        0x9100_0210, // 16: add  x16, x16, thunk
        0x9000_0000, // 20: adrp x0, cell(extern)
        0xf940_0000, // 24: ldr  x0, [x0, cell(extern)]
        0x9000_0001, // 28: adrp x1, datum
        0x9100_0021, // 32: add  x1, x1, datum
        0x9400_0000, // 36: bl   thunk
        0x1400_0000, // 40: b    own + 8
    ]);
    let fixup = |at, kind, to, addend| Fixup { at, kind, to, addend };
    let fixups = [
        fixup(0, Kind::GotPage21, To::Place(0), 0),
        fixup(4, Kind::GotPageOffset12, To::Place(0), 0),
        fixup(12, Kind::AdrpPage21, To::Place(0), 0),
        fixup(16, Kind::PageOffset12, To::Place(0), 0),
        fixup(20, Kind::GotPage21, To::Place(1), 0),
        fixup(24, Kind::GotPageOffset12, To::Place(1), 0),
        fixup(28, Kind::AdrpPage21, To::Data(0), 0),
        fixup(32, Kind::PageOffset12, To::Data(0), 0),
        fixup(36, Kind::Call26, To::Place(0), 0),
        fixup(40, Kind::Call26, To::Own, 8),
    ];
    let places = [Place::Function, Place::Extern("write")];
    let datum = Datum { id: [7; 32], align: 8, bytes: b"hi", fixups: List::of(&[]), writable: false };
    let data = [datum];
    let record = code(Arch::Aarch64, &bytes, &fixups, &places, &data);
    let pool = pool(&record, KEPT).expect("laid out");
    // 44 bytes of code, to 48, and one cell.
    assert_eq!(pool.len, 56);
    let bounds = [Bound::Function { word: WORD, thunk: THUNK }, Bound::At(EXTERN)];
    let mut out = Vec::new();
    place(&record, &pool, AT, &bounds, &[DATUM], &mut out).expect("placed");
    assert_eq!(out.len(), 56);
    let placed = words(&out[..44]);
    let adrp = |word: u32, at: u64| -> u64 {
        let pages = (((word >> 5) & 0x7_ffff) << 2 | ((word >> 29) & 3)) as i64;
        let pages = (pages << 43) >> 43;
        ((at & !0xfff) as i64 + (pages << 12)) as u64
    };
    let lo12 = |word: u32| u64::from((word >> 10) & 0xfff);
    // The call: the slot word, through its page and its offset as an 8-byte load scales it.
    assert_eq!(adrp(placed[0], AT) + lo12(placed[1]) * 8, WORD);
    // The address: the thunk.
    assert_eq!(adrp(placed[3], AT + 12) + lo12(placed[4]), THUNK);
    // The outside symbol: the pool's cell, which holds its address.
    let cell = AT + 48;
    assert_eq!(adrp(placed[5], AT + 20) + lo12(placed[6]) * 8, cell);
    assert_eq!(out[48..56], EXTERN.to_le_bytes());
    // The datum, directly.
    assert_eq!(adrp(placed[7], AT + 28) + lo12(placed[8]), DATUM);
    // A direct branch to a function goes to its thunk; one into the code itself, there.
    let branch = |word: u32, at: u64| (at as i64 + ((((word & 0x03ff_ffff) as i64) << 38) >> 36)) as u64;
    assert_eq!(branch(placed[9], AT + 36), THUNK);
    assert_eq!(branch(placed[10], AT + 40), AT + 8);
    // No bits but a field's changed: every opcode is as it was.
    assert_eq!(placed[2], 0xd63f_0200);
    assert_eq!(placed[9] >> 26, 0x9400_0000 >> 26);

    // What cannot be encoded is refused, naming the fixup: a thunk past a branch's reach.
    let far = [Bound::Function { word: WORD, thunk: AT + (1 << 28) }, Bound::At(EXTERN)];
    let refused = place(&record, &pool, AT, &far, &[DATUM], &mut out).unwrap_err();
    assert_eq!(refused.at, Some(36), "{refused}");
    // And a bind that gives too few places.
    assert!(place(&record, &pool, AT, &bounds[..1], &[DATUM], &mut out).is_err());
}

/// The same on x86-64: `call [rip + word]` reads the slot word, `lea` takes the thunk, a GOT
/// load of an outside symbol reads a pool cell; and a thread-local, on each instruction set
/// in its object format's way, reaches the launcher's thunk through the pool.
#[test]
fn an_x86_64_record_and_thread_locals_are_placed_through_the_pool() {
    const AT: u64 = 0x7000_1000;
    const WORD: u64 = 0x7000_0008;
    const THUNK: u64 = 0x7000_0800;
    let mut bytes = vec![0xff, 0x15, 0, 0, 0, 0]; //  0: call [rip + word]
    bytes.extend([0x48, 0x8d, 0x05, 0, 0, 0, 0]); //  6: lea rax, [rip + thunk]
    bytes.extend([0x48, 0x8b, 0x05, 0, 0, 0, 0]); // 13: mov rax, [rip + cell]
    bytes.extend([0x48, 0x8b, 0x3d, 0, 0, 0, 0]); // 20: mov rdi, [rip + tlv cell]
    let fixup = |at, kind, to| Fixup { at, kind, to, addend: -4 };
    let fixups = [
        fixup(2, Kind::GotPcRel32, To::Place(0)),
        fixup(9, Kind::PcRel32, To::Place(0)),
        fixup(16, Kind::GotPcRel32, To::Place(1)),
        fixup(23, Kind::TlvRel32, To::Place(2)),
    ];
    let places = [Place::Function, Place::Weak("gone"), Place::Static];
    let record = code(Arch::X86_64, &bytes, &fixups, &places, &[]);
    let pool = pool(&record, KEPT).expect("laid out");
    // 27 bytes of code, to 32; a cell; a thread-local's cell and descriptor.
    assert_eq!(pool.len, 32 + 8 + 32);
    let bounds = [
        Bound::Function { word: WORD, thunk: THUNK },
        Bound::At(0),
        Bound::Tls { thunk: 0xaaaa, arg: 5 },
    ];
    let mut out = Vec::new();
    place(&record, &pool, AT, &bounds, &[], &mut out).expect("placed");
    let rel = |at: usize| {
        let field = i32::from_le_bytes([out[at], out[at + 1], out[at + 2], out[at + 3]]);
        (AT as i64 + at as i64 + 4 + i64::from(field)) as u64
    };
    assert_eq!(rel(2), WORD);
    assert_eq!(rel(9), THUNK);
    assert_eq!(rel(16), AT + 32);
    assert_eq!(out[32..40], 0u64.to_le_bytes(), "a weak symbol nothing defines is 0");
    // The thread-local's cell holds the address of the descriptor after it.
    assert_eq!(rel(23), AT + 40);
    let word = |at: usize| u64::from_le_bytes(out[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!([word(40), word(48), word(56), word(64)], [AT + 48, 0xaaaa, 5, 0]);

    // ELF on AArch64: the descriptor pair itself, named by `adrp`, `ldr`, `add`.
    let bytes = le(&[0x9000_0000, 0xf940_0001, 0x9100_0000, 0xd63f_0020]);
    let to = To::Place(0);
    let fixups = [
        Fixup { at: 0, kind: Kind::TlsDescPage21, to, addend: 0 },
        Fixup { at: 4, kind: Kind::TlsDescLd64Lo12, to, addend: 0 },
        Fixup { at: 8, kind: Kind::TlsDescAddLo12, to, addend: 0 },
        Fixup { at: 12, kind: Kind::TlsDescCall, to, addend: 0 },
    ];
    let record = code(Arch::Aarch64, &bytes, &fixups, &[Place::Static], &[]);
    let pool = self::pool(&record, KEPT).expect("laid out");
    assert_eq!(pool.len, 32);
    let bounds = [Bound::Tls { thunk: 0xbbbb, arg: 9 }];
    place(&record, &pool, AT, &bounds, &[], &mut out).expect("placed");
    let placed = words(&out[..16]);
    assert_eq!(u64::from((placed[1] >> 10) & 0xfff) * 8, (AT + 16) & 0xfff);
    assert_eq!(u64::from((placed[2] >> 10) & 0xfff), (AT + 16) & 0xfff);
    let word = |at: usize| u64::from_le_bytes(out[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!([word(16), word(24)], [0xbbbb, 9]);
    // A thread-local access of a static that is not one is refused, naming the fixup.
    let refused = place(&record, &pool, AT, &[Bound::At(1)], &[], &mut out).unwrap_err();
    assert_eq!(refused.at, Some(0), "{refused}");
}

/// A static's value: its bytes, or its size in zeroes, each fixup an absolute address (a
/// function's is its thunk, so a `fn` pointer in a static follows the slot).
#[test]
fn a_statics_value_holds_thunks_and_addresses() {
    let bytes = [0u8; 24];
    let fixups = [
        Fixup { at: 0, kind: Kind::Abs64, to: To::Place(0), addend: 0 },
        Fixup { at: 8, kind: Kind::Abs64, to: To::Place(1), addend: 16 },
        Fixup { at: 16, kind: Kind::Abs64, to: To::Data(0), addend: 0 },
    ];
    let places = [Place::Function, Place::Static];
    let datum = Datum { id: [1; 32], align: 1, bytes: b"x", fixups: List::of(&[]), writable: false };
    let data = [datum];
    let record = Record {
        what: What::Static { mutable: true, thread_local: false },
        size: 24,
        ..code(Arch::Aarch64, &bytes, &fixups, &places, &data)
    };
    let bounds = [Bound::Function { word: 8, thunk: 0x4000 }, Bound::At(0x9000)];
    let mut out = Vec::new();
    place_static(&record, &bounds, &[0x5000], &mut out).expect("placed");
    let word = |at: usize| u64::from_le_bytes(out[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!([word(0), word(8), word(16)], [0x4000, 0x9010, 0x5000]);

    let zeroed = Record { size: 4096, bytes: &[], fixups: List::of(&[]), ..record };
    place_static(&zeroed, &bounds, &[0x5000], &mut out).expect("placed");
    assert_eq!(out, vec![0; 4096]);
    assert!(pool(&record, KEPT).is_err(), "a static was laid out as code");
}
