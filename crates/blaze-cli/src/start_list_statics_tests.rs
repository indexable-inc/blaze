//! Version 4 of the start list (rules 25, 26): a first answer is a group, its data, then the
//! statics it names ascending, then its code row; the bytes are a function of the rows alone;
//! every malformed group is a typed refusal named by what it is; and a list of an older form
//! is refused as such, nothing of it read. The oracle is the model rows below, never another
//! reader's answer.

use blaze_ids::ContentId;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::data::Datum;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AVec, Vec};

use super::*;

const TODAY: Certificate = Certificate::for_tests([0x7a; 32]);
const OTHER: Certificate = Certificate::for_tests([0x7b; 32]);

const HEAD_OF: StartHead = StartHead { line: EntryLine { form: EntryForm::Hosted, symbol: blaze_code::record::Symbol([0x5e; 32]) }, personality: None };

fn code(n: u8) -> Row {
    Row { symbol: Symbol([n; 32]), key: CodeKey(ContentId([n.wrapping_add(1); 32])), record: RecordId([n.wrapping_add(2); 32]) }
}

fn datum(n: u8) -> Datum {
    Datum { symbol: Symbol([n; 32]), record: RecordId([n.wrapping_add(3); 32]) }
}

/// The model run: fn 1 names nothing; fn 2 sent static 0x51 and names it; fn 3 sent 0x61 and
/// names 0x51 (sent by fn 2) and 0x61; fn 4 names nothing.
fn model() -> std::vec::Vec<Line> {
    std::vec![
        Line::Code(code(1)),
        Line::Datum(datum(0x51)),
        Line::Named(Symbol([0x51; 32])),
        Line::Code(code(2)),
        Line::Datum(datum(0x61)),
        Line::Named(Symbol([0x51; 32])),
        Line::Named(Symbol([0x61; 32])),
        Line::Code(code(3)),
        Line::Code(code(4)),
    ]
}

/// `lines` as a list under [`TODAY`], row by row (the planted writer, `encode_lines`): it
/// writes malformed groups too, for the reader's refusals.
fn bytes_of(lines: &[Line], alloc: Alloc<'_>) -> std::vec::Vec<u8> {
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode_lines(&TODAY, HEAD_OF, lines, &mut bytes).expect("the model fits");
    bytes.as_slice().to_vec()
}

/// The model's groups as the run writes them ([`encode`] over [`Group`]s): whole by type.
fn groups_bytes(alloc: Alloc<'_>) -> std::vec::Vec<u8> {
    let (one, six) = (Symbol([0x51; 32]), Symbol([0x61; 32]));
    let (two_data, three_data) = ([datum(0x51)], [datum(0x61)]);
    let (two_named, three_named) = ([one], [one, six]);
    let groups = [
        Group::code(code(1)),
        Group { data: &two_data, named: &two_named, values: &[], code: code(2) },
        Group { data: &three_data, named: &three_named, values: &[], code: code(3) },
        Group::code(code(4)),
    ];
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode(&TODAY, HEAD_OF, &groups, &mut bytes).expect("the model fits");
    bytes.as_slice().to_vec()
}

/// Asserts: the model's groups encode to the head and one 97-byte row each, read back as the
/// same rows in order, re-encode to equal bytes; under today's certificate the code rows and
/// the whole groups come back, under another only the code rows' symbols (a static is no need).
/// Prints "start list v4: 9 rows (4 code, 2 data, 3 names) round trip, equal bytes; another certificate yields 4 symbols".
#[test]
fn groups_round_trip_and_another_certificate_yields_code_symbols_only() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let model = model();
    let bytes = bytes_of(&model, alloc);
    assert_eq!(bytes.len(), HEAD + model.len() * 97, "a row is not its kind byte and three words");
    let view = read(&bytes, alloc).expect("the model reads");
    let lines: std::vec::Vec<Line> = view.lines().collect();
    assert_eq!(lines, model, "the rows read are not the rows written");
    assert_eq!(bytes_of(&lines, alloc), bytes, "a rewrite of the rows read is other bytes");
    // Finding 10: the groups' writer and the row-by-row writer lay equal bytes.
    assert_eq!(groups_bytes(alloc), bytes, "encoding the groups is other bytes than encoding their rows");
    let codes: std::vec::Vec<Row> = view.iter().collect();
    assert_eq!(codes, [code(1), code(2), code(3), code(4)]);
    let read_groups: std::vec::Vec<std::vec::Vec<Line>> = view.groups().map(|group| group.lines().collect()).collect();
    assert_eq!(read_groups.len(), 4, "the reader's groups are not one per code row: {read_groups:?}");
    assert_eq!(read_groups.concat(), model, "the reader's groups are not the rows in order");
    match view.under(&TODAY) {
        Held::Today(today) => {
            assert_eq!(today.lines().collect::<std::vec::Vec<_>>(), model);
            assert_eq!(today.head(), HEAD_OF);
        }
        Held::Other(_) => panic!("today's certificate read as another"),
    }
    match view.under(&OTHER) {
        Held::Other(symbols) => {
            let got: std::vec::Vec<Symbol> = symbols.iter().collect();
            assert_eq!(got, [Symbol([1; 32]), Symbol([2; 32]), Symbol([3; 32]), Symbol([4; 32])], "another certificate's list handed out a static or lost a fn");
        }
        Held::Today(_) => panic!("another certificate's rows were handed out with keys and records"),
    }
    std::println!("start list v4: 9 rows (4 code, 2 data, 3 names) round trip, equal bytes; another certificate yields 4 symbols");
}

/// Asserts: `Rows::push_answer` keeps a group whole or refuses it whole by its code row's
/// symbol: with 3 rows left under the cap, a group of 2 data, 1 name and its row (4) is
/// refused and nothing of it is kept; a group of 3 fits exactly.
/// Prints "start list v4: a group past the cap refused whole by its row's symbol".
#[test]
fn a_group_past_the_cap_is_refused_whole() {
    let frame = bstd::mem::Frame::with_reserve(1 << 24, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut rows = Rows::new_in(alloc);
    for n in 0..(MOST_ROWS - 3) as u32 {
        let mut symbol = [0u8; 32];
        symbol[..4].copy_from_slice(&n.to_le_bytes());
        symbol[31] = 0xcc;
        rows.push(Row { symbol: Symbol(symbol), key: CodeKey(ContentId(symbol)), record: RecordId(symbol) }).expect("under the cap");
    }
    let before = rows.len();
    let big = rows.push_answer(&[datum(0x51), datum(0x61)], &[Symbol([0x51; 32])], &[], code(9));
    assert_eq!(big, Err(CapError::Full { symbol: code(9).symbol }), "a group past the cap was taken, or refused without its row's name");
    assert_eq!(rows.len(), before, "a refused group left rows behind");
    rows.push_answer(&[datum(0x51)], &[Symbol([0x51; 32])], &[], code(8)).expect("a group of exactly the room left fits");
    assert!(rows.is_full());
    std::println!("start list v4: a group past the cap refused whole by its row's symbol");
}

/// Asserts: every cut of the model is `Short` or `Length`; a count past the cap is `TooMany`;
/// a kind byte of 7 is `Kind` naming the row; a named row with a stray byte is `Padding`; a
/// datum after a named row, and names not ascending, are `Order`; data with no code row after
/// them is `Dangling`; a static's datum in two rows is `DatumTwice`. A reader that trusted the
/// rows would place a group the child could not stand on.
/// Prints "start list v4: cuts, the cap, an unknown kind, padding, order, a dangling group and a datum twice each refused by name".
#[test]
fn a_malformed_group_is_a_typed_error() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let whole = bytes_of(&model(), alloc);
    for cut in 0..whole.len() {
        match read(&whole[..cut], alloc) {
            Err(ReadError::Short { len }) => assert!(cut < HEAD && len == cut),
            Err(ReadError::Length { len, want }) => assert!(cut >= HEAD && len == cut && want == whole.len()),
            other => panic!("a list cut at {cut} of {} read as {other:?}", whole.len()),
        }
    }
    let mut count = whole.clone();
    count[12..16].copy_from_slice(&(MOST_ROWS as u32 + 1).to_le_bytes());
    assert_eq!(read(&count, alloc).map(|_| ()), Err(ReadError::TooMany { rows: MOST_ROWS as u32 + 1 }));
    let mut kind = whole.clone();
    kind[HEAD + ROW] = 7;
    assert_eq!(read(&kind, alloc).map(|_| ()), Err(ReadError::Kind { row: 1, found: 7 }), "a row of no kind was read");
    let mut padding = whole.clone();
    // Row 2 is a named row: its key word must be zero.
    padding[HEAD + 2 * ROW + 40] = 1;
    assert_eq!(read(&padding, alloc).map(|_| ()), Err(ReadError::Padding { row: 2 }), "a named row with stray bytes was read");
    let late = [Line::Named(Symbol([0x51; 32])), Line::Datum(datum(0x51)), Line::Code(code(1))];
    assert_eq!(read(&bytes_of(&late, alloc), alloc).map(|_| ()), Err(ReadError::Order { row: 1 }), "a datum after a name was read");
    let unsorted = [Line::Datum(datum(0x51)), Line::Datum(datum(0x61)), Line::Named(Symbol([0x61; 32])), Line::Named(Symbol([0x51; 32])), Line::Code(code(1))];
    assert_eq!(read(&bytes_of(&unsorted, alloc), alloc).map(|_| ()), Err(ReadError::Order { row: 3 }), "names not ascending were read");
    let dangling = [Line::Code(code(1)), Line::Datum(datum(0x51))];
    assert_eq!(read(&bytes_of(&dangling, alloc), alloc).map(|_| ()), Err(ReadError::Dangling { rows: 1 }), "a datum with no code row after it was read");
    let twice = [Line::Datum(datum(0x51)), Line::Code(code(1)), Line::Datum(datum(0x51)), Line::Code(code(2))];
    assert_eq!(read(&bytes_of(&twice, alloc), alloc).map(|_| ()), Err(ReadError::DatumTwice { symbol: Symbol([0x51; 32]) }), "a static's datum in two rows was read");
    std::println!("start list v4: cuts, the cap, an unknown kind, padding, order, a dangling group and a datum twice each refused by name");
}

/// Asserts (rule 26): a list of version 3, the form before data rows (96-byte rows, no kind
/// byte), is `Old` naming its version, before its length or rows are looked at, and says it is
/// an older form that this run rewrites; a version past today's is `Version`, not `Old`.
/// Prints "start list v4: a version 3 list reported as an older form, not read".
#[test]
fn a_version_3_list_is_an_older_form_and_not_read() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    // A version 3 list of one row as that form wrote it: the head, then symbol, key, record.
    let mut three = bytes_of(&[] as &[Line], alloc);
    three[8..12].copy_from_slice(&3u32.to_le_bytes());
    three[12..16].copy_from_slice(&1u32.to_le_bytes());
    let row = code(1);
    three.extend_from_slice(&row.symbol.0);
    three.extend_from_slice(&row.key.0.0);
    three.extend_from_slice(&row.record.0);
    let refused = read(&three, alloc).map(|_| ());
    assert_eq!(refused, Err(ReadError::Old { found: 3 }), "a version 3 list was read, or refused as something else");
    let said = std::format!("{}", ReadError::Old { found: 3 });
    assert!(said.contains("version 3, an older form") && said.contains(&std::format!("writes version {VERSION}")), "{said}");
    let mut newer = bytes_of(&model(), alloc);
    newer[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
    assert_eq!(read(&newer, alloc).map(|_| ()), Err(ReadError::Version { found: VERSION + 1 }));
    std::println!("start list v4: a version 3 list reported as an older form, not read");
}
