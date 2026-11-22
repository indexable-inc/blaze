//! The four tests of drive.md's "A finding inside an expansion is reported where its tokens
//! came from" (ruled 20:57), on the column's core: a finding's range in an expansion's text,
//! mapped through the column read back from its bytes, lands on the user's text as written.
//! The calling texts are real Rust and are lexed by the expander's own lexer (`lex_placed`);
//! only the expansions' texts are written by hand, token by token, as a renderer writes them.
//!
//! Expected lines (each test prints one):
//! - `origin io: "io: Io"`
//! - `origin sol: "uint256" inside the braces`
//! - `origin def: at the call (Def)`
//! - `origin control: three at the call`

extern crate std;

use blaze_own::{AString, AVec, Alloc};

use crate::origin_column::{InputShape, Mapped, NotInput, OriginColumn, OriginsRef, TextOrigin, input_places, map_range};
use crate::Edition;

/// An expansion's text from `tokens`, one space between two, and its column's bytes.
fn render<'a>(tokens: &[(&str, TextOrigin)], alloc: Alloc<'a>) -> (AString<'a>, AVec<'a, u8>) {
    let mut text = AString::new_in(alloc);
    let mut column = OriginColumn::new_in(alloc);
    for (i, (token, origin)) in tokens.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start = text.len();
        text.push_str(token);
        column.push(start, text.len(), *origin, None).expect("rows in order");
    }
    let mut bytes: AVec<'a, u8> = AVec::new_in(alloc);
    column.write(&mut bytes).expect("a small column");
    (text, bytes)
}

/// The byte range of the first `needle` in `text`, as u32s.
fn range_of(text: &str, needle: &str) -> (u32, u32) {
    let start = text.find(needle).expect("the needle is in the text");
    (start as u32, (start + needle.len()) as u32)
}

/// The input tokens' bytes in `input` for a mapped range.
fn input_bytes(input: &str, shape: InputShape, mapped: Mapped, alloc: Alloc<'_>) -> (usize, usize) {
    let Mapped::Input { first, last } = mapped else { panic!("mapped to the call: {mapped:?}") };
    let places = input_places(input, shape, Edition::E2021, alloc).expect("the input lexes");
    (places[first as usize].0 as usize, places[last as usize].1 as usize)
}

/// The index of the token `token` in `input` lexed as `shape`.
fn index_of(input: &str, shape: InputShape, token: &str, alloc: Alloc<'_>) -> u32 {
    let places = input_places(input, shape, Edition::E2021, alloc).expect("the input lexes");
    places.iter().position(|&(s, e)| &input[s as usize..e as usize] == token).expect("the token is lexed") as u32
}

const MAIN: &str = "fn main(cx: &Cx, io: Io) {}";

/// D104's refusal of `fn main(cx: &Cx, io: Io)`: the launcher's expansion copies the parameter
/// `io: Io` from its input (the implicit `#[std::main]`'s input is the whole item); a finding on
/// those tokens is filed at `io: Io` in the user's file, not at bytes of the launcher's text.
fn launcher<'a>(alloc: Alloc<'a>) -> (AString<'a>, AVec<'a, u8>) {
    let at = |t| TextOrigin::Input(index_of(MAIN, InputShape::Whole, t, alloc));
    let made = TextOrigin::Made;
    render(
        &[
            ("fn", made),
            ("bstd_hosted_start", made),
            ("(", made),
            (")", made),
            ("{", made),
            ("let", made),
            ("io", at("io")),
            (":", TextOrigin::Input(index_of(MAIN, InputShape::Whole, "io", alloc) + 1)),
            ("Io", at("Io")),
            ("=", made),
            ("grant", made),
            ("(", made),
            (")", made),
            (";", made),
            ("}", made),
        ],
        alloc,
    )
}

#[test]
fn io_parameter_is_reported_at_io_io() {
    let alloc = blaze_own::testing::heap().alloc();
    let (text, bytes) = launcher(alloc);
    let (column, rest) = OriginsRef::open(&bytes).expect("the column reads back");
    assert!(rest.is_empty());
    let (start, end) = range_of(&text, "io : Io");
    let mapped = map_range(column, start, end);
    let (s, e) = input_bytes(MAIN, InputShape::Whole, mapped, alloc);
    std::println!("origin io: {:?}", &MAIN[s..e]);
    assert_eq!(&MAIN[s..e], "io: Io");
}

const SOL: &str = "sol! { function transfer(address to, uint256 amount) returns (bool); }";

/// The text between `SOL`'s braces, and where it starts in `SOL`.
fn sol_input() -> (&'static str, usize) {
    let open = SOL.find('{').expect("a brace") + 1;
    let close = SOL.rfind('}').expect("a brace");
    (&SOL[open..close], open)
}

/// A `sol!`-like expansion: the call's types and names copied into a struct the macro makes.
fn sol_expansion<'a>(alloc: Alloc<'a>) -> (AString<'a>, AVec<'a, u8>) {
    let (input, _) = sol_input();
    let at = |t| TextOrigin::Input(index_of(input, InputShape::Inside, t, alloc));
    let made = TextOrigin::Made;
    render(
        &[
            ("pub", made),
            ("struct", made),
            ("transferCall", made),
            ("{", made),
            ("pub", made),
            ("to", at("to")),
            (":", made),
            ("Address", made),
            (",", made),
            ("pub", made),
            ("amount", at("amount")),
            (":", made),
            ("uint256", at("uint256")),
            (",", made),
            ("}", made),
        ],
        alloc,
    )
}

#[test]
fn sol_input_error_is_reported_inside_the_braces() {
    let alloc = blaze_own::testing::heap().alloc();
    let (text, bytes) = sol_expansion(alloc);
    let (column, _) = OriginsRef::open(&bytes).expect("the column reads back");
    let (start, end) = range_of(&text, "uint256");
    let mapped = map_range(column, start, end);
    let (input, input_at) = sol_input();
    let (s, e) = input_bytes(input, InputShape::Inside, mapped, alloc);
    let (s, e) = (input_at + s, input_at + e);
    let inside = SOL.find('{').expect("a brace") < s && e < SOL.rfind('}').expect("a brace");
    std::println!("origin sol: {:?}{}", &SOL[s..e], if inside { " inside the braces" } else { " OUTSIDE the braces" });
    assert_eq!(&SOL[s..e], "uint256");
    assert!(inside);
}

/// A macro whose finding is wholly in its own tokens: `-> u8` from the definition.
fn def_expansion<'a>(alloc: Alloc<'a>) -> (AString<'a>, AVec<'a, u8>) {
    let made = TextOrigin::Made;
    render(&[("fn", TextOrigin::Def(0)), ("helper", made), ("(", made), (")", made), ("->", TextOrigin::Def(3)), ("u8", TextOrigin::Def(4)), ("{", made), ("0", made), ("}", made)], alloc)
}

#[test]
fn own_tokens_are_reported_at_the_call() {
    let alloc = blaze_own::testing::heap().alloc();
    let (text, bytes) = def_expansion(alloc);
    let (column, _) = OriginsRef::open(&bytes).expect("the column reads back");
    let (start, end) = range_of(&text, "u8");
    let mapped = map_range(column, start, end);
    std::println!("origin def: at the call ({:?})", match mapped {
        Mapped::Call(NotInput::Def) => "Def",
        _ => "NOT Def",
    });
    assert_eq!(mapped, Mapped::Call(NotInput::Def));
    // A range of input and made tokens together is not wholly the user's: the call.
    let (text, bytes) = launcher(alloc);
    let (column, _) = OriginsRef::open(&bytes).expect("the column reads back");
    let (start, end) = range_of(&text, "let io : Io =");
    assert_eq!(map_range(column, start, end), Mapped::Call(NotInput::Made));
}

/// Control: with the origin column dropped, all three findings are filed at the call.
#[test]
fn control_without_the_column_all_three_are_at_the_call() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut at_call = 0;
    for (text, needle) in [(launcher(alloc).0, "io : Io"), (sol_expansion(alloc).0, "uint256"), (def_expansion(alloc).0, "u8")] {
        let (start, end) = range_of(&text, needle);
        if map_range(OriginsRef::EMPTY, start, end) == Mapped::Call(NotInput::NoToken) {
            at_call += 1;
        }
    }
    std::println!("origin control: {} at the call", if at_call == 3 { "three" } else { "NOT three" });
    assert_eq!(at_call, 3);
}

/// The identity column of a text the lexer gives tokens SHARING a place (tempo census g051231:
/// "origin row 1 (0..39) is out of order after one ending at 39", a kept `/// doc` +
/// `#[derive(..)]` item): a doc comment's `# [ doc = ".." ]`, a lifetime's `'` and name. One row a
/// place, its first token's; it opens and maps each shape back to its bytes. Control: one row a
/// token, as `kept` wrote it, is refused AT THE WRITE naming both rows.
#[test]
fn shared_places_are_one_row_each() {
    use crate::origin_column::ColumnError;
    let alloc = blaze_own::testing::heap().alloc();
    let text = "/// Doc.\n#[derive(Show)]\npub struct N<'a> { n: &'a u8 }";
    let places = input_places(text, InputShape::Whole, Edition::E2021, alloc).expect("the text lexes");
    let column = OriginColumn::input_identity(&places, alloc).expect("one row a place is sorted");
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    column.write(&mut bytes).expect("a small column");
    let (read, _) = OriginsRef::open(&bytes).expect("the column reads back");
    assert!(read.len() < places.len(), "tokens share places: {} rows for {} tokens", read.len(), places.len());
    for (needle, from) in [("/// Doc.", 0), ("'a", text.find("n:").expect("the field")), ("#[derive(Show)]", 0)] {
        let at = from + text[from..].find(needle).expect("the needle");
        let (s, e) = input_bytes(text, InputShape::Whole, map_range(read, at as u32, (at + needle.len()) as u32), alloc);
        assert_eq!(&text[s..e], needle, "`{needle}` maps to its own bytes");
    }
    // Control: a row a token.
    let mut per_token = OriginColumn::new_in(alloc);
    let refused = places.iter().enumerate().try_for_each(|(i, &(s, e))| per_token.push(s as usize, e as usize, TextOrigin::Input(i as u32), None));
    assert_eq!(refused, Err(ColumnError::Unsorted { row: 1, start: 0, end: 8, previous_start: 0, previous_end: 8 }));
    std::println!("origin shared: {} rows for {} tokens; a row a token refused", read.len(), places.len());
}

/// The same column keeps each token's mark (drive.md, "A token keeps its mark", ruled 21:27): two
/// definition tokens of one `macro` share one table entry, a written token has none, and the
/// table's bytes do not depend on the order the marks were met in. Prints
/// `origin marks: 1 kept, read back equal`.
#[test]
fn marks_ride_in_the_same_column() {
    use crate::origin_column::TextMark;
    use crate::{DefSite, Transparency};
    use blaze_id::{ContentId, CrateItemKey, ItemKey};
    let alloc = blaze_own::testing::heap().alloc();
    let site = DefSite { krate: CrateItemKey(ContentId([7; 32])), module: Some(ItemKey(ContentId([9; 32]))) };
    let opaque = TextMark { site, transparency: Transparency::Opaque, edition: Edition::E2021 };
    let other = TextMark { site: DefSite { krate: CrateItemKey(ContentId([3; 32])), module: None }, transparency: Transparency::SemiOpaque, edition: Edition::E2024 };
    let bytes_of = |marks: [Option<TextMark>; 3]| {
        let mut column = OriginColumn::new_in(alloc);
        column.push(0, 2, TextOrigin::Def(0), marks[0]).expect("in order");
        column.push(3, 9, TextOrigin::Input(4), marks[1]).expect("in order");
        column.push(10, 12, TextOrigin::Def(1), marks[2]).expect("in order");
        let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
        column.write(&mut bytes).expect("a small column");
        bytes
    };
    let bytes = bytes_of([Some(opaque), None, Some(opaque)]);
    let (column, _) = OriginsRef::open(&bytes).expect("the column reads back");
    assert_eq!((column.mark(0), column.mark(1), column.mark(2)), (Some(opaque), None, Some(opaque)));
    assert_eq!(bytes.len(), 8 + crate::origin_column::MARK + 3 * crate::origin_column::ROW, "one mark kept once");
    // Two marks met in either order: equal bytes.
    let (a, b) = (bytes_of([Some(opaque), None, Some(other)]), bytes_of([Some(other), None, Some(opaque)]));
    let (ra, _) = OriginsRef::open(&a).expect("reads");
    assert_eq!((ra.mark(0), ra.mark(2)), (Some(opaque), Some(other)));
    assert_eq!(a[..8 + 2 * crate::origin_column::MARK], b[..8 + 2 * crate::origin_column::MARK]);
    std::println!("origin marks: 1 kept, read back equal");
}
