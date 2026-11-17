//! P8b's tests (UNWIND.md 3.8): LLVM's LSDA read back into blaze's unwind rows. Prints, when
//! whole: "read_lsda: blaze's and LLVM's tables read to the same rows".
//!
//! The rule, from the pinned library's personality routine (library/std/src/sys/personality/
//! dwarf/eh.rs, nightly-2026-10-27, `find_eh_action` and `interpret_cs_action`): "if cs_lpad
//! == 0 { return Ok(EHAction::None) }" (unwinding goes on: `Continue`); "If cs_action_entry is
//! 0 then this is a cleanup (Drop::drop)" (`Cleanup`); an action record with "ttype_index > 0"
//! is "Stop unwinding Rust panics at catch_unwind" (`Catch`), a negative one `EHAction::Filter`,
//! which the routine turns into ending the program for a Rust panic (`Terminate`). A wrong
//! action is a wrong program: a `Cleanup` read as `Continue` skips the drops of everything
//! live, a `Catch` read as `Terminate` aborts where `catch_unwind` must return `Err`, so every
//! test compares every row, action and landing included.

use blaze_code::eh::{self, Placed};
use blaze_code::unwind::{self, Action, Frame, Row};
use blaze_llvm::read_lsda::{LsdaError, LsdaRefusal, Plant, rows_of_lsda, rows_of_lsda_planted};
use blaze_own::{AVec, Alloc, Vec};

const ENTRY_AT: u64 = 0x7000_0000;
const CODE_AT: u64 = 0x1_0000_4000;
const PERSONALITY: u64 = 0x1_0000_0100;

/// `rows`' LSDA as blaze-code eh.rs makes it at placement: the public `eh::entry`, sliced at
/// `Entry::lsda` (`lsda_table` is private there).
fn blaze_lsda(rows: &[Row], code_len: u32, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let unwind = unwind::of_rows(Some(Frame::PLAIN), rows, code_len).expect("the rows are well formed");
    let mut out: AVec<'static, u8> = Vec::new_in(alloc);
    let placed = Placed { entry_at: ENTRY_AT, code_at: CODE_AT, personality: Some(PERSONALITY) };
    let made = eh::entry(&unwind, placed, &mut out).expect("an entry for rows that land");
    let at = made.lsda.expect("a row lands, so the entry has an LSDA");
    let mut lsda: AVec<'static, u8> = Vec::new_in(alloc);
    lsda.extend_from_slice(&out[at..]);
    lsda
}

fn row(start: u32, len: u32, landing: u32, action: Action) -> Row {
    Row { start, len, landing, action }
}

/// Row sets covering each action alone and together (so the writer's type table is present,
/// absent, holds only the catch-all, only the empty filter, or both). `Frame::PLAIN`'s prologue
/// is 8 bytes: every call and landing is past it, on a 4-byte instruction.
fn sets() -> [(&'static [Row], u32); 5] {
    const ALL: [Row; 5] = [
        Row { start: 8, len: 4, landing: 0, action: Action::Continue },
        Row { start: 12, len: 4, landing: 40, action: Action::Cleanup },
        Row { start: 16, len: 4, landing: 44, action: Action::Catch },
        Row { start: 20, len: 4, landing: 48, action: Action::Terminate },
        Row { start: 28, len: 4, landing: 52, action: Action::Catch },
    ];
    const CLEANUP: [Row; 2] = [
        Row { start: 8, len: 4, landing: 32, action: Action::Cleanup },
        Row { start: 16, len: 8, landing: 0, action: Action::Continue },
    ];
    const CATCH: [Row; 1] = [Row { start: 12, len: 4, landing: 24, action: Action::Catch }];
    const TERMINATE: [Row; 2] = [
        Row { start: 8, len: 4, landing: 24, action: Action::Terminate },
        Row { start: 12, len: 4, landing: 28, action: Action::Terminate },
    ];
    // Offsets past 0x80 and 0x4000, where a ULEB128 would take two and three bytes.
    const FAR: [Row; 2] = [
        Row { start: 0x90, len: 4, landing: 0x4100, action: Action::Cleanup },
        Row { start: 0x4004, len: 4, landing: 0x4200, action: Action::Catch },
    ];
    [(&ALL, 64), (&CLEANUP, 64), (&CATCH, 32), (&TERMINATE, 32), (&FAR, 0x4400)]
}

/// LLVM's form on arm64 (`EHStreamer::emitExceptionTable`): call sites in ULEB128, the type
/// table `DW_EH_PE_indirect | DW_EH_PE_pcrel | DW_EH_PE_sdata4` aligned to 4 bytes, a region
/// of plain calls with no landing as its own row, the catch-all as the null entry of type 1
/// and `filter [0 x ptr] zeroinitializer` as filter -1 naming the empty list at the base.
/// Hand-built from the Itanium C++ ABI's exception table layout, byte by byte:
///
/// | at | bytes | what |
/// |---|---|---|
/// | 0 | `ff` | landing-pad base omitted |
/// | 1 | `9b` | type table: indirect pcrel sdata4 |
/// | 2 | `1d` | type table base: 29 bytes past this field's end (3), at 32 |
/// | 3 | `01` | call sites in ULEB128 |
/// | 4 | `11` | 17 bytes of call sites |
/// | 5 | `10 04 40 00` | [0x10, +4): land at 0x40, cleanup |
/// | 9 | `14 08 00 00` | [0x14, +8): no landing |
/// | 13 | `1c 04 48 01` | [0x1c, +4): land at 0x48, action record 1 |
/// | 17 | `24 04 80 04 03` | [0x24, +4): land at 0x200, action record 3 |
/// | 22 | `01 00` | action record 1: type 1, no next |
/// | 24 | `7f 00` | action record 3: type -1 (a filter), no next |
/// | 26 | `00 00` | alignment of the type table to 4 |
/// | 28 | `00 00 00 00` | type 1: null, the catch-all |
/// | 32 | `00` | the base: filter -1's exception specification, empty |
const LLVM_ULEB: [u8; 33] = [
    0xff, 0x9b, 0x1d, 0x01, 0x11, //
    0x10, 0x04, 0x40, 0x00, //
    0x14, 0x08, 0x00, 0x00, //
    0x1c, 0x04, 0x48, 0x01, //
    0x24, 0x04, 0x80, 0x04, 0x03, //
    0x01, 0x00, 0x7f, 0x00, //
    0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0x00,
];
const LLVM_ULEB_LEN: u32 = 0x400;

fn llvm_uleb_rows() -> [Row; 4] {
    [
        row(0x10, 4, 0x40, Action::Cleanup),
        row(0x14, 8, 0, Action::Continue),
        row(0x1c, 4, 0x48, Action::Catch),
        row(0x24, 4, 0x200, Action::Terminate),
    ]
}

/// LLVM's udata4 call sites with no type table (only cleanups): `ff ff 03`, 26 bytes of call
/// sites, each three little-endian `u32`s and a ULEB128 action entry, then four bytes of the
/// next function's table, which the reader must not take as this one's.
const LLVM_UDATA4: [u8; 34] = [
    0xff, 0xff, 0x03, 0x1a, //
    0x08, 0, 0, 0, 0x04, 0, 0, 0, 0x30, 0, 0, 0, 0x00, //
    0x0c, 0, 0, 0, 0x10, 0, 0, 0, 0x00, 0, 0, 0, 0x00, //
    0xff, 0xff, 0x03, 0x0d,
];
const LLVM_UDATA4_LEN: u32 = 0x40;

fn llvm_udata4_rows() -> [Row; 2] {
    [row(0x08, 4, 0x30, Action::Cleanup), row(0x0c, 0x10, 0, Action::Continue)]
}

fn refusal(lsda: &[u8], func_len: u32) -> LsdaRefusal {
    match rows_of_lsda(lsda, CODE_AT, func_len, Alloc::global()) {
        Ok(rows) => panic!("refusal expected, read {:?}", rows.as_slice()),
        Err(LsdaError { func_start, refusal, .. }) => {
            assert_eq!(func_start, CODE_AT, "the error names the function");
            refusal
        }
    }
}

/// Round trip: for every set, eh.rs's LSDA reads back to the very rows it was made of; LLVM's
/// own two forms read to the rows the table says, byte by byte.
#[test]
fn blaze_and_llvm_tables_read_to_the_same_rows() {
    let alloc = Alloc::global();
    for (rows, code_len) in sets() {
        let lsda = blaze_lsda(rows, code_len, alloc);
        let read = rows_of_lsda(&lsda, CODE_AT, code_len, alloc).expect("eh.rs's LSDA reads");
        assert_eq!(read.as_slice(), rows, "eh.rs's LSDA of {rows:?}");
    }
    let read = rows_of_lsda(&LLVM_ULEB, CODE_AT, LLVM_ULEB_LEN, alloc).expect("LLVM's ULEB128 form reads");
    assert_eq!(read.as_slice(), &llvm_uleb_rows());
    let read = rows_of_lsda(&LLVM_UDATA4, CODE_AT, LLVM_UDATA4_LEN, alloc).expect("LLVM's udata4 form reads");
    assert_eq!(read.as_slice(), &llvm_udata4_rows());
    std::println!("read_lsda: blaze's and LLVM's tables read to the same rows");
}

/// The control (rule 53): with a catch-all action record planted to read as a terminate, the
/// comparison above fails on every input holding a catch, and only there.
#[test]
fn the_oracle_catches_a_wrong_action_mapping() {
    let alloc = Alloc::global();
    for (rows, code_len) in sets() {
        let lsda = blaze_lsda(rows, code_len, alloc);
        let read = rows_of_lsda_planted(&lsda, CODE_AT, code_len, alloc, Plant::CatchReadsAsTerminate).expect("reads");
        let has_catch = rows.iter().any(|r| r.action == Action::Catch);
        assert_eq!(read.as_slice() != rows, has_catch, "the plant shows exactly where a catch is: {rows:?}");
    }
    let read = rows_of_lsda_planted(&LLVM_ULEB, CODE_AT, LLVM_ULEB_LEN, alloc, Plant::CatchReadsAsTerminate).expect("reads");
    assert_ne!(read.as_slice(), &llvm_uleb_rows(), "the plant is caught on LLVM's form");
    assert_eq!(read[2], row(0x1c, 4, 0x48, Action::Terminate));
}

/// Every form outside the mapping is refused by name, never read as a near guess.
#[test]
fn forms_outside_the_mapping_are_refused_by_name() {
    // A catch of a named type: type 1's entry not null.
    let mut typed = LLVM_ULEB;
    typed[28] = 0x10;
    assert_eq!(refusal(&typed, LLVM_ULEB_LEN), LsdaRefusal::TypedCatch { start: 0x1c, index: 1, entry: 0x10 });
    // A chained action record: record 1's next is 2.
    let mut chained = LLVM_ULEB;
    chained[23] = 0x02;
    assert_eq!(refusal(&chained, LLVM_ULEB_LEN), LsdaRefusal::ChainedAction { start: 0x1c, next: 2 });
    // A filter naming a type: the specification at the base lists type 1.
    let mut filter = LLVM_ULEB;
    filter[32] = 0x01;
    assert_eq!(refusal(&filter, LLVM_ULEB_LEN), LsdaRefusal::NonEmptyFilter { start: 0x24, index: -1, first: 1 });
    // The record (0, 0): a cleanup said through the action table.
    let mut zero = LLVM_ULEB;
    zero[22] = 0x00;
    assert_eq!(refusal(&zero, LLVM_ULEB_LEN), LsdaRefusal::CleanupRecord { start: 0x1c });
    // A call site past the function: [0x10, 0x14) in an 0x12-byte function.
    assert_eq!(refusal(&LLVM_ULEB, 0x12), LsdaRefusal::CallSiteOutside { start: 0x10, len: 4, func_len: 0x12 });
    // A landing past the function: 0x40 in a 0x40-byte function.
    assert_eq!(refusal(&LLVM_ULEB, 0x40), LsdaRefusal::LandingOutside { start: 0x10, landing: 0x40, func_len: 0x40 });
    // An encoded landing-pad base.
    let mut lp_start = LLVM_ULEB;
    lp_start[0] = 0x00;
    assert_eq!(refusal(&lp_start, LLVM_ULEB_LEN), LsdaRefusal::LpStartEncoding { encoding: 0x00 });
    // Call sites in sdata4.
    let mut encoding = LLVM_ULEB;
    encoding[3] = 0x0b;
    assert_eq!(refusal(&encoding, LLVM_ULEB_LEN), LsdaRefusal::CallSiteEncoding { encoding: 0x0b });
    // Unsorted call sites: the second row starts at 0x0c, before the first's end at 0x14.
    let mut unsorted = LLVM_ULEB;
    unsorted[9] = 0x0c;
    assert_eq!(refusal(&unsorted, LLVM_ULEB_LEN), LsdaRefusal::CallSitesUnsorted { start: 0x0c, previous_end: 0x14 });
    // A truncated table: the last byte (filter -1's specification, at the base) cut off.
    assert_eq!(refusal(&LLVM_ULEB[..32], LLVM_ULEB_LEN), LsdaRefusal::TypeOutside { start: 0x24, index: -1 });
    // A type table base past the bytes: the offset raised by one.
    let mut base = LLVM_ULEB;
    base[2] = 0x1f;
    assert_eq!(refusal(&base, LLVM_ULEB_LEN), LsdaRefusal::TypesOutside { offset: 0x1f, lsda: 33 });
}
