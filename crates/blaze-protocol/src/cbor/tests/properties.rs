//! The differential tests above as properties, so a failure shrinks to its smallest shape:
//! every shape writes the reference's bytes, which check and read as the reference reads them,
//! and so do those bytes cut, flipped, grown and swapped.

use hegel::{Hegel, Settings, TestCase};
use hegel::generators as gs;

use super::{NAMES, Shape, same_reading};
use crate::cbor::{check, reference, to_vec};

fn text(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("aZ0 \u{e9}\u{6c34}\u{1f525}").max_size(11))
}

fn name(tc: &TestCase) -> usize {
    tc.draw(gs::integers().max_value(NAMES.len() - 1))
}

/// Mostly a few, now and then past CBOR's one-byte lengths.
fn count(tc: &TestCase) -> usize {
    if tc.draw(gs::weighted_booleans(0.025)) {
        tc.draw(gs::integers().min_value(24).max_value(323))
    } else {
        tc.draw(gs::integers().max_value(5))
    }
}

fn fields(tc: &TestCase, depth: u32) -> Vec<(usize, Shape)> {
    let len: usize = tc.draw(gs::integers().max_value(6));
    (0..len).map(|_| (name(tc), shape(tc, depth - 1))).collect()
}

fn shape(tc: &TestCase, depth: u32) -> Shape {
    let leaf = depth == 0 || tc.draw(gs::weighted_booleans(1.0 / 3.0));
    let kinds = if leaf { gs::integers().max_value(7) } else { gs::integers().min_value(8).max_value(13) };
    let kind: u8 = tc.draw(kinds);
    match kind {
        0 => Shape::Unsigned(tc.draw(gs::integers())),
        1 => Shape::Signed(tc.draw(gs::integers())),
        2 => Shape::Wide(tc.draw(gs::integers())),
        3 => Shape::Float(tc.draw(gs::floats())),
        4 => Shape::Bool(tc.draw(gs::booleans())),
        5 => Shape::Null,
        6 => Shape::Text(text(tc)),
        7 => Shape::Bytes(tc.draw(gs::binary().max_size(39))),
        8 => {
            let items = (0..count(tc)).map(|_| shape(tc, depth - 1)).collect();
            Shape::Seq { said: tc.draw(gs::booleans()), items }
        }
        9 => {
            let len: usize = tc.draw(gs::integers().max_value(4));
            Shape::Tuple((0..len).map(|_| shape(tc, depth - 1)).collect())
        }
        10 => {
            let n = count(tc);
            let said = match tc.draw(gs::integers::<u8>().max_value(2)) {
                0 => None,
                1 => Some(n),
                _ => Some(tc.draw(gs::integers().max_value(29))),
            };
            let entries = (0..n).map(|_| (shape(tc, 0), shape(tc, depth - 1))).collect();
            Shape::Map { said, entries }
        }
        11 => Shape::Struct(fields(tc, depth)),
        12 => {
            let name = name(tc);
            let of = tc.draw(gs::booleans()).then(|| Box::new(shape(tc, depth - 1)));
            Shape::Variant { name, of }
        }
        _ => Shape::StructVariant { name: name(tc), fields: fields(tc, depth) },
    }
}

fn mutate(tc: &TestCase, bytes: &mut Vec<u8>) {
    for _ in 0..=tc.draw(gs::integers::<u8>().max_value(3)) {
        match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 if !bytes.is_empty() => {
                let at = tc.draw(gs::integers().max_value(bytes.len() - 1));
                bytes[at] ^= 1 << tc.draw(gs::integers::<u8>().max_value(7));
            }
            1 if !bytes.is_empty() => {
                let at = tc.draw(gs::integers().max_value(bytes.len() - 1));
                bytes[at] = tc.draw(gs::integers());
            }
            2 => bytes.truncate(tc.draw(gs::integers().max_value(bytes.len()))),
            3 => {
                let at = tc.draw(gs::integers().max_value(bytes.len()));
                bytes.insert(at, tc.draw(gs::integers()));
            }
            _ if bytes.len() >= 2 => {
                let at = tc.draw(gs::integers().max_value(bytes.len() - 2));
                bytes.swap(at, at + 1);
            }
            _ => bytes.push(0),
        }
    }
}

fn writes_and_reads_as_the_reference_does(tc: TestCase) {
    let shape = shape(&tc, 4);
    let new = to_vec(&shape);
    assert_eq!(new, reference::to_vec(&shape), "{shape:?}");
    if let Ok(mut bytes) = new {
        check(&bytes).unwrap();
        same_reading(&bytes);
        mutate(&tc, &mut bytes);
        same_reading(&bytes);
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn every_shape_writes_and_reads_as_the_reference_does() {
    run_cases(500, writes_and_reads_as_the_reference_does);
}
