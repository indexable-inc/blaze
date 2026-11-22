//! The start list's form and its root (rules 25, 26): bytes are a function of the rows and the
//! certificate alone; what is read back is what was written; a list under another certificate
//! yields its symbols and nothing else; the cap and every malformed input are typed refusals
//! named by what they are; and writing a list equal to the one held writes nothing. The oracle is
//! the model rows `row(n)` (rule 8), never another reader's answer.


use blaze_cas::plain::{Lent, Settings as StoreSettings};
use blaze_core::FileName;
use blaze_core::sim::{Faults, Sim};
use blaze_ids::ContentId;
use blaze_ladder::code_records::CodeKey;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AVec, Vec};

use super::*;
use crate::store::plain_place::{Facts, StoreAlloc, after_answer, config, open, test_cx};

/// Model row `n`: every word distinct from every other row's.
fn row(n: u32) -> Row {
    let word = |what: u8| {
        let mut out = [what; 32];
        out[..4].copy_from_slice(&n.to_le_bytes());
        out
    };
    Row { symbol: Symbol(word(1)), key: CodeKey(ContentId(word(2))), record: RecordId(word(3)) }
}

fn rows(count: u32) -> std::vec::Vec<Row> {
    (0..count).map(row).collect()
}

fn process(sim: Sim, n: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let cfg = config(FileName::new("store").expect("a name"), &StoreSettings::default(), Facts { boot: Some(1), nonce: 9000 + n }, alloc);
    open(sim, cfg, alloc).expect("the store opens")
}

fn back(lent: Lent<Sim, StoreAlloc>) -> Sim {
    lent.into_store().into_driver()
}

const TODAY: Certificate = Certificate::for_tests([0x7a; 32]);
const OTHER: Certificate = Certificate::for_tests([0x7b; 32]);

/// The model run's head: a Rust start's shim, and a personality routine.
const ENTRY: StartHead = StartHead {
    line: EntryLine { form: EntryForm::CMain, symbol: blaze_code::record::Symbol([0x5e; 32]) },
    personality: Some(blaze_code::record::Symbol([0x9e; 32])),
};

/// Asserts: encode then read gives the certificate and every row in order; encoding the read
/// rows again gives equal bytes (deterministic); under today's certificate the rows come back
/// with keys and records, under another only the symbols, in the list's order.
/// Prints "start list: 300 rows round trip, equal bytes on rewrite; another certificate yields 300 symbols only".
#[test]
fn rows_round_trip_and_another_certificate_yields_symbols_only() {
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let model = rows(300);
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, ENTRY, &model, &mut bytes).expect("300 rows fit");
    assert_eq!(bytes.len(), HEAD + 300 * ROW);
    let view = read(bytes.as_slice(), alloc).expect("a list reads");
    let back: std::vec::Vec<Row> = view.iter().collect();
    assert_eq!(back, model, "the rows read are not the rows written");
    let mut again: AVec<'_, u8> = Vec::new_in(alloc);
    // The certificate written is read back: the list is today's under it (below), another's
    // under any other (the last match).
    let entry = match view.under(&TODAY) {
        Held::Today(today) => {
            assert_eq!(today.iter().collect::<std::vec::Vec<_>>(), model);
            today.head()
        }
        Held::Other(_) => panic!("today's certificate read as another"),
    };
    assert_eq!(entry, ENTRY, "the entry line read is not the one written");
    encode_lines(&TODAY, entry, &back, &mut again).expect("again");
    assert_eq!(again.as_slice(), bytes.as_slice(), "a rewrite of the rows read is other bytes");
    match view.under(&OTHER) {
        Held::Other(symbols) => {
            let got: std::vec::Vec<Symbol> = symbols.iter().collect();
            let want: std::vec::Vec<Symbol> = model.iter().map(|row| row.symbol).collect();
            assert_eq!(got, want, "another certificate's symbols are not the list's, in order");
        }
        Held::Today(_) => panic!("another certificate's rows were handed out with keys and records: old code could be placed"),
    }
    std::println!("start list: 300 rows round trip, equal bytes on rewrite; another certificate yields 300 symbols only");
}

/// Asserts: `Rows` takes 8,192 rows and refuses the 8,193rd naming its symbol; `encode` of
/// 8,193 rows refuses the same row; a list of exactly 8,192 rows reads.
/// Prints "start list: 8192 rows kept, row 8193 refused by its symbol".
#[test]
fn the_cap_refuses_row_8193_by_name() {
    let frame = bstd::mem::Frame::with_reserve(1 << 24, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut kept = Rows::new_in(alloc);
    for n in 0..MOST_ROWS as u32 {
        kept.push(row(n)).expect("under the cap");
    }
    assert!(kept.is_full());
    let past = row(MOST_ROWS as u32);
    assert_eq!(kept.push(past), Err(CapError::Full { symbol: past.symbol }), "row 8,193 was taken or refused without its name");
    let mut over = rows(MOST_ROWS as u32);
    over.push(past);
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    assert_eq!(encode_lines(&TODAY, ENTRY, &over, &mut bytes), Err(CapError::Full { symbol: past.symbol }));
    assert!(bytes.is_empty(), "a refused encode wrote bytes");
    kept.encode(&TODAY, ENTRY, &mut bytes).expect("the cap itself fits");
    assert_eq!(bytes.len(), LONGEST);
    assert_eq!(read(bytes.as_slice(), alloc).expect("a full list reads").len(), MOST_ROWS);
    std::println!("start list: 8192 rows kept, row 8193 refused by its symbol");
}

/// Asserts: every cut of a list short of its length is `Short` (inside the head) or `Length`
/// (inside the rows); a byte past its end is `Length`; input past `LONGEST` is `Oversized`
/// before anything is read; another magic, another version, a count past the cap and a symbol
/// in two rows are each their own refusal. A reader that trusted the head's count would read
/// past the bytes of a cut list.
/// Prints "start list: every truncation and overlong input refused by kind".
#[test]
fn truncated_or_oversized_input_is_a_typed_error() {
    let frame = bstd::mem::Frame::with_reserve(1 << 23, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, ENTRY, &rows(3), &mut bytes).expect("3 rows");
    let whole = bytes.as_slice();
    for cut in 0..whole.len() {
        match read(&whole[..cut], alloc) {
            Err(ReadError::Short { len }) => assert!(cut < HEAD && len == cut),
            Err(ReadError::Length { len, want }) => assert!(cut >= HEAD && len == cut && want == whole.len()),
            other => panic!("a list cut at {cut} of {} read as {other:?}", whole.len()),
        }
    }
    let mut long = std::vec::Vec::from(whole);
    long.push(0);
    assert_eq!(read(&long, alloc).map(|_| ()), Err(ReadError::Length { len: whole.len() + 1, want: whole.len() }));
    let huge = std::vec![0u8; LONGEST + 1];
    assert_eq!(read(&huge, alloc).map(|_| ()), Err(ReadError::Oversized { len: LONGEST + 1 }));
    let mut magic = std::vec::Vec::from(whole);
    magic[0] ^= 1;
    assert!(matches!(read(&magic, alloc), Err(ReadError::Magic { .. })));
    let mut version = std::vec::Vec::from(whole);
    version[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
    assert_eq!(read(&version, alloc).map(|_| ()), Err(ReadError::Version { found: VERSION + 1 }));
    // The version just before today's, derived from the constant (rule 26): an old home's list
    // is reported as old at its head and remade, never read on to fail at an entry.
    let mut older = std::vec::Vec::from(whole);
    older[8..12].copy_from_slice(&(VERSION - 1).to_le_bytes());
    assert_eq!(read(&older, alloc).map(|_| ()), Err(ReadError::Old { found: VERSION - 1 }), "the previous version's list was read, or refused as something else");
    let mut count = std::vec::Vec::from(whole);
    count[12..16].copy_from_slice(&(MOST_ROWS as u32 + 1).to_le_bytes());
    assert_eq!(read(&count, alloc).map(|_| ()), Err(ReadError::TooMany { rows: MOST_ROWS as u32 + 1 }));
    // The first word no launcher form has, derived from the one table of forms (`word_form`),
    // never a literal: a form added later moves it.
    let unknown = (0u32..).find(|&word| word_form(word).is_none()).expect("a word no form has");
    let mut form = std::vec::Vec::from(whole);
    form[48..52].copy_from_slice(&unknown.to_le_bytes());
    assert_eq!(read(&form, alloc).map(|_| ()), Err(ReadError::Form { found: unknown }), "an entry form no launcher has was read");
    let mut word = std::vec::Vec::from(whole);
    word[84..88].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(read(&word, alloc).map(|_| ()), Err(ReadError::Personality { found: 2 }), "a personality word past 1 was read");
    let mut stray = std::vec::Vec::from(whole);
    stray[84..88].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(read(&stray, alloc).map(|_| ()), Err(ReadError::Personality { found: 0 }), "no routine with a symbol that is not zero was read");
    let none = StartHead { personality: None, ..ENTRY };
    let mut unnamed: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, none, &rows(1), &mut unnamed).expect("1 row");
    match read(unnamed.as_slice(), alloc).expect("a list naming no routine").under(&TODAY) {
        Held::Today(today) => assert_eq!(today.head(), none, "a head naming no routine read back as another"),
        Held::Other(_) => panic!("today's certificate read as another"),
    }
    let mut twice: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, ENTRY, &[row(1), row(2), row(1)], &mut twice).expect("the encoder does not dedup");
    assert_eq!(read(twice.as_slice(), alloc).map(|_| ()), Err(ReadError::Twice { symbol: row(1).symbol }));
    std::println!("start list: every truncation and overlong input refused by kind");
}

/// Asserts, over the command's one store on the simulation: no list before one is written; a
/// list written by one process is read in the next with equal bytes; writing equal bytes again
/// is `Wrote::Nothing` and leaves the store with nothing uncommitted and no put counted; writing
/// other bytes (another certificate) is `Written`, and the next process reads the newer one (the
/// pointer's newest commit wins). A writer that put and pointed every time would count a put.
/// Prints "start list store: written once, equal list writes nothing, the newer list wins".
#[test]
fn an_equal_list_writes_nothing_and_the_newer_wins() {
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let name = ListName::of(&ContentId::of("start list test shape", b"run hello"));
    let mut first: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, ENTRY, &rows(5), &mut first).expect("5 rows");
    let cx = test_cx();
    let lent = process(Sim::new(1, Faults::default()), 1);
    assert_eq!(held(&lent, &name).expect("a read"), None, "a list before any was written");
    assert_eq!(write(&lent, &name, None, first.as_slice()).expect("a write"), Wrote::Written);
    after_answer(&cx, &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 2);
    let kept = held(&lent, &name).expect("a read").expect("the list written");
    assert_eq!(kept, first.as_slice(), "the next process read other bytes");
    let puts = lent.counts().puts;
    assert_eq!(write(&lent, &name, Some(kept), first.as_slice()).expect("a write"), Wrote::Nothing);
    assert_eq!((lent.counts().puts, lent.uncommitted_bytes()), (puts, 0), "an equal list wrote");
    let mut second: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&OTHER, ENTRY, &rows(2), &mut second).expect("2 rows");
    assert_eq!(write(&lent, &name, Some(kept), second.as_slice()).expect("a write"), Wrote::Written);
    after_answer(&cx, &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 3);
    assert_eq!(held(&lent, &name).expect("a read"), Some(second.as_slice()), "the newer list did not win");
    let other = ListName::of(&ContentId::of("start list test shape", b"test hello"));
    assert_eq!(held(&lent, &other).expect("a read"), None, "another shape's name read this one's list");
    std::println!("start list store: written once, equal list writes nothing, the newer list wins");
}
