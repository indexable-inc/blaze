//! The squeezed record's promises: every record comes back as it went in, with its dictionary or
//! none; a reader without the dictionary is refused by its name; corrupt bytes are refused or
//! decode to the very record (a flip in a bit nothing reads); truncated bytes and bytes after the
//! frame are refused; a size over the limit is refused before the output grows; and each oracle
//! catches the bug its control plants.

use std::alloc::Global;
use std::sync::LazyLock;
use std::vec::Vec;
use std::{format, vec};

use blaze_sim::{Broke, Config, Tape};

use super::plant::{self, Plant};
use super::{DictId, Dictionary, Decoder, Encoder, Refused, needs};

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

/// SplitMix64: bytes from a seed, so a case's record is one choice.
fn mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A record the shape of a scope record: a header, then rows of a name, a 32-byte id from a
/// small set, and small fields; `seed` picks the rows.
fn record(seed: u64, rows: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = b"BSR2\x00\x01".to_vec();
    for _ in 0..rows {
        let r = mix(&mut s);
        let name = ["into_iter", "from", "fmt", "Option", "Result", "clone", "eq", "len"][(r % 8) as usize];
        out.extend_from_slice(name.as_bytes());
        let mut id = (r >> 3) % 24;
        for _ in 0..4 {
            out.extend_from_slice(&mix(&mut id).to_le_bytes());
        }
        out.extend_from_slice(&[(r >> 8) as u8 & 3, (r >> 16) as u8 & 7, 0, 1]);
    }
    out
}

/// One dictionary for every case, trained on records of the shape `record` makes.
static DICT: LazyLock<Dictionary<Global>> = LazyLock::new(|| {
    let samples: Vec<Vec<u8>> = (0..200).map(|i| record(i, 20 + (i as usize % 40))).collect();
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    Dictionary::train(&refs, 16 << 10, Global).unwrap_or_else(|e| panic!("training the test dictionary: {e}"))
});

/// A record of a drawn shape: empty, shaped like the dictionary's, or noise.
fn draw_record(tape: &Tape) -> Vec<u8> {
    let seed = tape.next_u64("seed");
    match tape.below(4, "shape") {
        0 => vec![],
        1 => {
            let mut s = seed;
            (0..tape.int_in(1usize..=3000, "noise len")).map(|_| mix(&mut s) as u8).collect()
        }
        _ => record(seed, tape.int_in(0usize..=120, "rows")),
    }
}

struct Case {
    input: Vec<u8>,
    squeezed: Vec<u8>,
    dict: bool,
}

fn squeeze(tape: &Tape) -> Result<Case, Broke> {
    let input = draw_record(tape);
    let dict = tape.one_in(2, "with the dictionary");
    let level = tape.int_in(1i32..=19, "level");
    let mut enc = Encoder::new(level, dict.then_some(&*DICT)).map_err(|e| oracle(format!("encoder refused: {e}")))?;
    let mut squeezed = Vec::new();
    enc.squeeze(&input, &mut squeezed).map_err(|e| oracle(format!("squeeze refused: {e}")))?;
    Ok(Case { input, squeezed, dict })
}

fn ours(id: DictId) -> Option<&'static Dictionary<Global>> {
    (id == DICT.id()).then_some(&*DICT)
}

fn unsqueeze(bytes: &[u8], limit: usize, out: &mut Vec<u8>) -> Result<(), Refused> {
    Decoder::new()?.unsqueeze(bytes, ours, limit, out)
}

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let case = squeeze(tape)?;
    let named = needs(&case.squeezed).map_err(|e| oracle(format!("round trip: the header refused: {e}")))?;
    if named != case.dict.then(|| DICT.id()) {
        return Err(oracle(format!("round trip: the record names {named:?}")));
    }
    let mut out = vec![7u8; 3];
    unsqueeze(&case.squeezed, case.input.len(), &mut out).map_err(|e| oracle(format!("round trip: refused: {e}")))?;
    if out[..3] != [7; 3] || out[3..] != case.input[..] {
        return Err(oracle(format!("round trip: {} bytes came back for {}", out.len() - 3, case.input.len())));
    }
    if case.dict {
        let mut out = Vec::new();
        let none = |_: DictId| -> Option<&'static Dictionary<Global>> { None };
        match Decoder::new().map_err(|e| oracle(format!("{e}")))?.unsqueeze(&case.squeezed, none, usize::MAX, &mut out) {
            Err(Refused::MissingDictionary(id)) if id == DICT.id() => {}
            got => return Err(oracle(format!("round trip: without its dictionary the read gave {got:?}"))),
        }
    }
    Ok(())
}

/// The invariant: every squeezed record round-trips only with its exact dictionary, refuses trailing or corrupt bytes, and rejects decoded sizes over the limit before growing the output.
#[test]
fn every_record_comes_back_with_its_dictionary_or_none() {
    blaze_sim::check(Config::cases(300), round_trip);
}

#[test]
fn the_oracle_catches_a_record_naming_another_dictionary() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || round_trip(tape));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::NamesOtherDictionary, "round trip", run);
}

fn damaged(tape: &Tape) -> Result<(), Broke> {
    let case = squeeze(tape)?;
    let mut bytes = case.squeezed.clone();
    let mut out = Vec::new();
    match tape.below(3, "damage") {
        // A byte changed: refused, or the very record.
        0 => {
            let at = tape.index(bytes.len(), "at");
            bytes[at] ^= tape.int_in(1u8..=255, "flip");
            match unsqueeze(&bytes, 1 << 20, &mut out) {
                Err(_) => {}
                Ok(()) if out == case.input => {}
                Ok(()) => return Err(oracle(format!("corrupt: byte {at} changed and other bytes came back"))),
            }
        }
        // Cut short: refused.
        1 => {
            bytes.truncate(tape.index(bytes.len(), "cut"));
            if let Ok(()) = unsqueeze(&bytes, 1 << 20, &mut out) {
                return Err(oracle(format!("truncated: {} of {} bytes were read as a record", bytes.len(), case.squeezed.len())));
            }
        }
        // Bytes after the frame: refused by count.
        _ => {
            let extra = tape.int_in(1usize..=40, "extra");
            bytes.extend((0..extra).map(|i| i as u8));
            match unsqueeze(&bytes, 1 << 20, &mut out) {
                Err(Refused::Trailing { bytes: n }) if n == extra => {}
                got => return Err(oracle(format!("trailing: {extra} bytes after the frame gave {got:?}"))),
            }
        }
    }
    Ok(())
}

#[test]
fn corrupt_truncated_and_trailing_bytes_are_refused() {
    blaze_sim::check(Config::cases(500), damaged);
}

#[test]
fn the_oracle_catches_trailing_bytes_kept() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || damaged(tape));
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::TrailingKept, "trailing", run);
}

fn over_the_limit(tape: &Tape) -> Result<(), Broke> {
    let case = squeeze(tape)?;
    if case.input.is_empty() {
        return Ok(());
    }
    let limit = tape.index(case.input.len(), "limit");
    let mut out: Vec<u8> = Vec::new();
    match unsqueeze(&case.squeezed, limit, &mut out) {
        Err(Refused::TooLarge { size, limit: l }) if size == case.input.len() as u64 && l == limit && out.capacity() == 0 => Ok(()),
        got => Err(oracle(format!(
            "limit: {} bytes under a limit of {limit} gave {got:?}, the output holding {} bytes of room",
            case.input.len(),
            out.capacity()
        ))),
    }
}

#[test]
fn a_size_over_the_limit_is_refused_before_the_output_grows() {
    blaze_sim::check(Config::cases(300), over_the_limit);
}

#[test]
fn the_oracle_catches_the_limit_ignored() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || over_the_limit(tape));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::LimitIgnored, "limit", run);
}

#[test]
fn a_dictionary_is_named_by_its_bytes() {
    let again = Dictionary::from_record(DICT.bytes().to_vec()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(again.id(), DICT.id());
    let mut other = DICT.bytes().to_vec();
    let last = other.len() - 1;
    other[last] ^= 1;
    let other = Dictionary::from_record(other).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(other.id(), DICT.id());
    assert_eq!(Dictionary::from_record(b"not a dictionary".to_vec()).err(), Some(Refused::Zstd { what: "not a zstd dictionary" }));
}
