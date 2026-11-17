//! The differential tests above as properties, so a failure shrinks to its smallest shape:
//! every shape writes the reference's bytes, which check and read as the reference reads them,
//! and so do those bytes cut, flipped, grown and swapped.

use blaze_sim::{Config, Tape};

use super::{NAMES, Shape, same_reading};
use crate::cbor::{check, reference, to_vec};

fn text(tape: &Tape) -> String {
    tape.text_from("aZ0 \u{e9}\u{6c34}\u{1f525}", 0..=11, "text")
}

fn name(tape: &Tape) -> usize {
    tape.index(NAMES.len(), "name")
}

/// Mostly a few, now and then past CBOR's one-byte lengths.
fn count(tape: &Tape) -> usize {
    if tape.chance(0.025, "a long count") {
        tape.int_in(24usize..=323, "long count")
    } else {
        tape.int_in(0usize..=5, "count")
    }
}

fn fields(tape: &Tape, depth: u32) -> Vec<(usize, Shape)> {
    let len = tape.int_in(0usize..=6, "fields");
    (0..len).map(|_| (name(tape), shape(tape, depth - 1))).collect()
}

fn shape(tape: &Tape, depth: u32) -> Shape {
    let leaf = depth == 0 || tape.chance(1.0 / 3.0, "leaf");
    let kind: u8 = if leaf { tape.int_in(0u8..=7, "leaf kind") } else { tape.int_in(8u8..=13, "kind") };
    match kind {
        0 => Shape::Unsigned(tape.int::<u64>("unsigned")),
        1 => Shape::Signed(tape.int::<i64>("signed")),
        2 => Shape::Wide(tape.i128("wide")),
        3 => Shape::Float(tape.f64("float")),
        4 => Shape::Bool(tape.bool("bool")),
        5 => Shape::Null,
        6 => Shape::Text(text(tape)),
        7 => Shape::Bytes(tape.bytes(0..=39, "bytes")),
        8 => {
            let items = (0..count(tape)).map(|_| shape(tape, depth - 1)).collect();
            Shape::Seq { said: tape.bool("said"), items }
        }
        9 => {
            let len = tape.int_in(0usize..=4, "tuple len");
            Shape::Tuple((0..len).map(|_| shape(tape, depth - 1)).collect())
        }
        10 => {
            let n = count(tape);
            let said = match tape.int_in(0u8..=2, "said") {
                0 => None,
                1 => Some(n),
                _ => Some(tape.int_in(0usize..=29, "said len")),
            };
            let entries = (0..n).map(|_| (shape(tape, 0), shape(tape, depth - 1))).collect();
            Shape::Map { said, entries }
        }
        11 => Shape::Struct(fields(tape, depth)),
        12 => {
            let name = name(tape);
            let of = tape.bool("variant has a value").then(|| Box::new(shape(tape, depth - 1)));
            Shape::Variant { name, of }
        }
        _ => Shape::StructVariant { name: name(tape), fields: fields(tape, depth) },
    }
}

fn mutate(tape: &Tape, bytes: &mut Vec<u8>) {
    for _ in 0..=tape.int_in(0u8..=3, "mutations") {
        match tape.int_in(0u8..=4, "mutation") {
            0 if !bytes.is_empty() => {
                let at = tape.index(bytes.len(), "flip at");
                bytes[at] ^= 1 << tape.int_in(0u8..=7, "flip bit");
            }
            1 if !bytes.is_empty() => {
                let at = tape.index(bytes.len(), "set at");
                bytes[at] = tape.int::<u8>("set to");
            }
            2 => bytes.truncate(tape.int_in(0usize..=bytes.len(), "cut to")),
            3 => {
                let at = tape.int_in(0usize..=bytes.len(), "insert at");
                bytes.insert(at, tape.int::<u8>("insert"));
            }
            _ if bytes.len() >= 2 => {
                let at = tape.index(bytes.len() - 1, "swap at");
                bytes.swap(at, at + 1);
            }
            _ => bytes.push(0),
        }
    }
}

fn writes_and_reads_as_the_reference_does(tape: &Tape) {
    let shape = shape(tape, 4);
    let new = to_vec(&shape);
    assert_eq!(new, reference::to_vec(&shape), "{shape:?}");
    if let Ok(mut bytes) = new {
        check(&bytes).unwrap();
        same_reading(&bytes);
        mutate(tape, &mut bytes);
        same_reading(&bytes);
    }
}

/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn every_shape_writes_and_reads_as_the_reference_does() {
    blaze_sim::check(Config::cases(500), writes_and_reads_as_the_reference_does);
}
