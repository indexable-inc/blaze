//! RFC 4648 section 10's vectors, both alphabets, every refusal by its offset, and the round
//! trip as a property with its planted-bug control.

use core::fmt;
use std::{format, string::String as StdString, vec::Vec as StdVec};

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::{
    Base64Plant, DecodeError, Engine, OutputTooSmall, Padding, STANDARD, STANDARD_NO_PAD,
    URL_SAFE, URL_SAFE_NO_PAD,
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn enc(engine: Engine, input: &[u8]) -> StdString {
    StdString::from(engine.encode_in(input, heap()).as_str())
}

fn dec(engine: Engine, input: &str) -> Result<StdVec<u8>, DecodeError> {
    engine.decode_in(input.as_bytes(), heap()).map(|v| v.as_slice().to_vec())
}

#[test]
fn rfc_4648_section_10_vectors() {
    let vectors: [(&str, &str, &str); 7] = [
        ("", "", ""),
        ("f", "Zg==", "Zg"),
        ("fo", "Zm8=", "Zm8"),
        ("foo", "Zm9v", "Zm9v"),
        ("foob", "Zm9vYg==", "Zm9vYg"),
        ("fooba", "Zm9vYmE=", "Zm9vYmE"),
        ("foobar", "Zm9vYmFy", "Zm9vYmFy"),
    ];
    for (plain, padded, unpadded) in vectors {
        assert_eq!(enc(STANDARD, plain.as_bytes()), padded, "{plain:?} padded");
        assert_eq!(enc(STANDARD_NO_PAD, plain.as_bytes()), unpadded, "{plain:?} unpadded");
        assert_eq!(dec(STANDARD, padded).unwrap(), plain.as_bytes(), "{padded:?}");
        assert_eq!(dec(STANDARD_NO_PAD, unpadded).unwrap(), plain.as_bytes(), "{unpadded:?}");
        assert_eq!(STANDARD.encoded_len(plain.len()), Some(padded.len()));
        assert_eq!(STANDARD_NO_PAD.encoded_len(plain.len()), Some(unpadded.len()));
    }
}

#[test]
fn the_url_safe_alphabet_differs_only_in_its_last_two_symbols() {
    let bytes = [0xFB, 0xFF, 0xBF];
    assert_eq!(enc(STANDARD, &bytes), "+/+/");
    assert_eq!(enc(URL_SAFE, &bytes), "-_-_");
    assert_eq!(enc(URL_SAFE_NO_PAD, &[0xFB, 0xFF]), "-_8");
    assert_eq!(enc(URL_SAFE, &[0xFB, 0xFF]), "-_8=");
    assert_eq!(dec(URL_SAFE, "-_-_").unwrap(), bytes);
    assert_eq!(dec(STANDARD, "-_-_"), Err(DecodeError::NotASymbol { at: 0, byte: b'-' }));
    assert_eq!(dec(URL_SAFE, "+/+/"), Err(DecodeError::NotASymbol { at: 0, byte: b'+' }));
}

#[test]
fn every_refusal_names_its_offset() {
    // A padded engine needs a multiple of four; an unpadded one never one past a multiple.
    assert_eq!(dec(STANDARD, "Zg"), Err(DecodeError::Length { len: 2 }));
    assert_eq!(dec(STANDARD, "Zm9vY"), Err(DecodeError::Length { len: 5 }));
    assert_eq!(dec(STANDARD_NO_PAD, "Zm9vY"), Err(DecodeError::Length { len: 5 }));
    // Padding where it may not be.
    assert_eq!(dec(STANDARD, "Z==="), Err(DecodeError::Padding { at: 1 }));
    assert_eq!(dec(STANDARD, "Zg==Zg=="), Err(DecodeError::Padding { at: 2 }));
    assert_eq!(dec(STANDARD, "===="), Err(DecodeError::Padding { at: 0 }));
    assert_eq!(dec(STANDARD_NO_PAD, "Zg=="), Err(DecodeError::Padding { at: 2 }));
    // Bytes outside the alphabet, a line break among them.
    assert_eq!(dec(STANDARD, "Zm9v\nYmFy"), Err(DecodeError::Length { len: 9 }));
    assert_eq!(dec(STANDARD, "Zm\n9"), Err(DecodeError::NotASymbol { at: 2, byte: b'\n' }));
    assert_eq!(dec(STANDARD, "Zm9\u{e9}"), Err(DecodeError::Length { len: 5 }));
    assert_eq!(dec(STANDARD, "Zm\u{e9}"), Err(DecodeError::NotASymbol { at: 2, byte: 0xC3 }));
    assert_eq!(dec(STANDARD_NO_PAD, "Zm\u{e9}"), Err(DecodeError::NotASymbol { at: 2, byte: 0xC3 }));
    // Bits past the last byte: "Zh==" and "Zm9=" mean what "Zg==" and "Zm8=" mean.
    assert_eq!(dec(STANDARD, "Zh=="), Err(DecodeError::TrailingBits { at: 1 }));
    assert_eq!(dec(STANDARD, "Zm9="), Err(DecodeError::TrailingBits { at: 2 }));
    assert_eq!(dec(STANDARD_NO_PAD, "Zh"), Err(DecodeError::TrailingBits { at: 1 }));
    // The caller's buffer.
    let mut small = [0u8; 2];
    assert_eq!(
        STANDARD.decode_into(b"Zm9v", &mut small),
        Err(DecodeError::OutputTooSmall(OutputTooSmall { need: 3, have: 2 }))
    );
    assert_eq!(STANDARD.encode_into(b"foo", &mut small), Err(OutputTooSmall { need: 4, have: 2 }));
    // Every refusal's text names the offset.
    let text = format!("{}", DecodeError::NotASymbol { at: 7, byte: b'!' });
    assert!(text.contains("offset 7") && text.contains("0x21"), "{text}");
}

#[test]
fn encode_append_pads_only_the_last_piece() {
    // Past 48 bytes the encoder works in pieces; only the whole input's end is padded.
    let input: std::vec::Vec<u8> = (0..=100u8).collect();
    let mut buf = [0u8; 200];
    let n = STANDARD.encode_into(&input, &mut buf).unwrap();
    assert_eq!(enc(STANDARD, &input).as_bytes(), &buf[..n]);
    assert_eq!(STANDARD.padding(), Padding::Padded);
}

const ENGINES: [Engine; 4] = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD];

/// The invariants of the round trip and of canonical decoding.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TripBroken {
    /// `encoded_len` is not the length of the encoding.
    Length { text: StdVec<u8>, said: Option<usize> },
    /// The encoding decoded to other bytes.
    Differs { bytes: StdVec<u8>, text: StdVec<u8>, back: StdVec<u8> },
    /// The encoding was refused by the decoder of the same engine.
    Refused { bytes: StdVec<u8>, text: StdVec<u8>, error: DecodeError },
    /// A text that decoded encodes back as another text: two texts mean the same bytes.
    NotCanonical { text: StdString, again: StdString },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::Length { text, said } => {
                write!(f, "encoded_len says {said:?}, not the length of {text:?}")
            }
            TripBroken::Differs { bytes, text, back } => {
                write!(f, "the round trip of {bytes:?} through {text:?} gave {back:?}")
            }
            TripBroken::Refused { bytes, text, error } => {
                write!(f, "the round trip of {bytes:?} through {text:?} was refused: {error}")
            }
            TripBroken::NotCanonical { text, again } => {
                write!(f, "{text:?} decoded, but encodes back as {again:?}")
            }
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the round trip, each with the invariant that must catch it.
    enum TripPlant breaks TripBroken {
        /// The encoder as it is.
        None,
        /// A two-byte tail's third symbol carries a stray bit: its decoder refuses it.
        TailSetsBit => TripBroken::Refused { error: DecodeError::TrailingBits { .. }, .. },
    }
}

impl TripPlant {
    /// The encoder's plant this control plants.
    fn planted(self) -> Base64Plant {
        match self {
            TripPlant::None => Base64Plant::None,
            TripPlant::TailSetsBit => Base64Plant::TailSetsBit,
        }
    }
}

/// The most bytes the round trip draws, and the room their encoding needs.
const MOST_BYTES: usize = 80;
const ENCODED_ROOM: usize = (MOST_BYTES + 2) / 3 * 4;

/// decode(encode(x)) == x, the length is `encoded_len`, and every symbol is the alphabet's.
fn round_trip(tape: &Tape, plant: impl Planted<Base64Plant>) -> Result<(), Broken<TripBroken>> {
    let bytes = tape.bytes(0..=MOST_BYTES, "bytes");
    let engine = *tape.one_of(&ENGINES, "engine");
    let mut room = [0u8; ENCODED_ROOM];
    let n = engine
        .encode_into_planted(&bytes, &mut room, plant)
        .map_err(|small| Broken::Harness(format!("the room for {} bytes is too small: {small}", bytes.len())))?;
    let text = &room[..n];
    if Some(n) != engine.encoded_len(bytes.len()) {
        let said = engine.encoded_len(bytes.len());
        return Err(Broken::Oracle(TripBroken::Length { text: text.to_vec(), said }));
    }
    match engine.decode_in(text, heap()) {
        Ok(back) if back.as_slice() == bytes.as_slice() => Ok(()),
        Ok(back) => Err(Broken::Oracle(TripBroken::Differs {
            bytes,
            text: text.to_vec(),
            back: back.as_slice().to_vec(),
        })),
        Err(error) => Err(Broken::Oracle(TripBroken::Refused { bytes, text: text.to_vec(), error })),
    }
}

#[test]
fn decode_inverts_encode() {
    check(Config::cases(500), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_round_trip_plant_is_caught_by_its_invariant() {
    controls(Config::cases(500), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

/// Any text of the alphabet's bytes and `=` either decodes and encodes back to itself (one
/// text per meaning) or is refused.
#[test]
fn a_text_that_decodes_is_the_canonical_one() {
    check(Config::cases(500), |tape| -> Result<(), Broken<TripBroken>> {
        let engine = *tape.one_of(&ENGINES, "engine");
        let text = tape.text_from("AQgw+/-_=", 0..=12, "text");
        match engine.decode_in(text.as_bytes(), heap()) {
            Err(_) => Ok(()),
            Ok(bytes) => {
                let again = engine.encode_in(&bytes, heap());
                match again.as_str() == text.as_str() {
                    true => Ok(()),
                    false => Err(Broken::Oracle(TripBroken::NotCanonical {
                        text: StdString::from(text.as_str()),
                        again: StdString::from(again.as_str()),
                    })),
                }
            }
        }
    });
}
