//! RFC 4648 section 10's vectors, both alphabets, every refusal by its offset, and the round
//! trip as a property with its planted-bug control.

use std::{alloc::Global, format, string::String as StdString};

use blaze_sim::{Broke, Config, Tape};

use crate::{
    DecodeError, Engine, OutputTooSmall, Padding, STANDARD, STANDARD_NO_PAD, URL_SAFE,
    URL_SAFE_NO_PAD,
    plant::{self, Plant},
};

fn enc(engine: Engine, input: &[u8]) -> StdString {
    StdString::from(engine.encode_in(input, Global).as_str())
}

fn dec(engine: Engine, input: &str) -> Result<std::vec::Vec<u8>, DecodeError> {
    engine.decode_in(input.as_bytes(), Global).map(|v| v.as_slice().to_vec())
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

/// decode(encode(x)) == x, the length is `encoded_len`, and every symbol is the alphabet's.
fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let bytes = tape.bytes(0..=80, "bytes");
    let engine = *tape.one_of(&ENGINES, "engine");
    let text = engine.encode_in(&bytes, Global);
    if Some(text.len()) != engine.encoded_len(bytes.len()) {
        return Err(Broke::Oracle(format!("encoded_len is not the length of {:?}", text.as_str())));
    }
    match engine.decode_in(text.as_bytes(), Global) {
        Ok(back) if back.as_slice() == bytes.as_slice() => Ok(()),
        Ok(back) => Err(Broke::Oracle(format!(
            "the round trip of {bytes:?} through {:?} gave {:?}",
            text.as_str(),
            back.as_slice()
        ))),
        Err(error) => Err(Broke::Oracle(format!(
            "the round trip of {bytes:?} through {:?} was refused: {error}",
            text.as_str()
        ))),
    }
}

#[test]
fn decode_inverts_encode() {
    blaze_sim::check(Config::cases(500), round_trip);
}

#[test]
fn the_oracle_catches_a_tail_with_a_stray_bit() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || round_trip(tape));
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::TailSetsBit, "round trip", run);
}

/// Any text of the alphabet's bytes and `=` either decodes and encodes back to itself (one
/// text per meaning) or is refused.
#[test]
fn a_text_that_decodes_is_the_canonical_one() {
    blaze_sim::check(Config::cases(500), |tape| {
        let engine = *tape.one_of(&ENGINES, "engine");
        let text = tape.text_from("AQgw+/-_=", 0..=12, "text");
        match engine.decode_in(text.as_bytes(), Global) {
            Err(_) => Ok(()),
            Ok(bytes) => {
                let again = engine.encode_in(&bytes, Global);
                match again.as_str() == text.as_str() {
                    true => Ok(()),
                    false => Err(Broke::Oracle(format!(
                        "{text:?} decoded, but encodes back as {:?}",
                        again.as_str()
                    ))),
                }
            }
        }
    });
}
