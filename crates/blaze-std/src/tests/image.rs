//! `bstd::image`: PNG decode and encode, and the deflate beneath them.
//!
//! - `decode(encode(img)) == img` for random images of every colour type at both depths,
//!   with pixels random, in runs and in ramps (so filters, matches and stored blocks all run).
//! - Deflate round-trips through the inflater for inputs of several blocks.
//! - `decode` of arbitrary bytes (random, behind a signature, and valid PNGs mutated with and
//!   without their CRCs fixed) ends in an image or an error, never a panic, and never asks its
//!   allocator for more than its limit; an image it does give is within the limits.
//! - Fixed vectors made by another encoder (Python's zlib, a dynamic-Huffman stream, every
//!   filter type, the stream split across IDAT chunks), this encoder's exact bytes for one
//!   image, and the refusals: a bad CRC, interlacing, a palette, a limit.
//!
//! Test-the-cause: the limit test shrinks the byte limit by one below what the image needs and
//! watches decode refuse it, where the exact limit decodes.

use std::alloc::Global;

use blaze_mem::{Frame, Tag};
use blaze_sim::{Config, Tape};

use crate::image::{
    Color, Depth, Image, ImageError, Limits, decode, decode_bytes, encode,
    zlib::{Crc, Inflate, compress},
};

/// What a frame may round its first allocation up by: the decode limit is checked against the
/// frame's peak with this much room.
const ALIGN_SLACK: usize = 64;

/// `len` bytes drawn from the tape: random, in runs, or a ramp.
fn content(tape: &Tape, len: usize, mode: u64) -> Vec<u8> {
    match mode {
        0 => (0..len).map(|_| tape.next_u64("byte") as u8).collect(),
        1 => {
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let byte = tape.next_u64("run byte") as u8;
                let run = 1 + tape.below(300, "run len") as usize;
                out.extend(std::iter::repeat_n(byte, run.min(len - out.len())));
            }
            out
        }
        _ => {
            let step = tape.below(7, "ramp step") as usize + 1;
            (0..len).map(|i| (i * step) as u8).collect()
        }
    }
}

fn draw_image(tape: &Tape) -> Image<Global> {
    let color = *tape.one_of(&Color::ALL, "color");
    let depth = *tape.one_of(&Depth::ALL, "depth");
    let width = tape.int_in(1..=24u64, "width") as u32;
    let height = tape.int_in(1..=24u64, "height") as u32;
    let len = (width * height * color.channels() * depth.bytes()) as usize;
    let mode = tape.int_in(0..=2u64, "content mode");
    let pixels = content(tape, len, mode);
    Image::new(width, height, color, depth, pixels).expect("a drawn image's pixels are its size")
}

#[test]
fn decode_of_encode_is_the_image() {
    blaze_sim::check(Config::cases(300), |tape| {
        let image = draw_image(tape);
        let png = encode(&image, Global).unwrap_or_else(|e| panic!("encode {image:?}: {e}"));
        let back = decode(&png, Global, Limits::DEFAULT).unwrap_or_else(|e| panic!("decode of encode {image:?}: {e}"));
        assert!(back == image, "decode(encode({image:?})) is {back:?}, with other pixels");
        let again = encode(&image, Global).unwrap_or_else(|e| panic!("encode {image:?} again: {e}"));
        assert_eq!(png, again, "encode {image:?} twice gave different bytes");
    });
}

#[test]
fn deflate_round_trips_through_inflate() {
    blaze_sim::check(Config::cases(60), |tape| {
        let len = tape.int_in(0..=100_000u64, "len") as usize;
        let mode = tape.int_in(0..=2u64, "content mode");
        let data = content(tape, len, mode);
        let z = compress(&data, Global).unwrap_or_else(|e| panic!("compress {len} bytes: {e}"));
        let mut out = vec![0u8; len + 1];
        let mut inflate = Inflate::new();
        inflate.feed(&z, false, &mut out).unwrap_or_else(|e| panic!("inflate {len} bytes: {e}"));
        assert!(inflate.done(), "the stream of {len} bytes did not end");
        assert_eq!(inflate.written(), len, "the stream of {len} bytes inflated to {}", inflate.written());
        assert!(out[..len] == data[..], "the stream of {len} bytes inflated to other bytes");
    });
}

/// Every chunk's CRC recomputed, as far as the chunks parse.
fn fix_crcs(bytes: &mut [u8]) {
    let mut at = 8;
    while at + 12 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let Some(end) = (at + 8).checked_add(len) else { return };
        if end + 4 > bytes.len() {
            return;
        }
        let mut sum = Crc::new();
        sum.update(&bytes[at + 4..end]);
        bytes[end..end + 4].copy_from_slice(&sum.finish().to_be_bytes());
        at = end + 4;
    }
}

fn hostile(tape: &Tape) -> Vec<u8> {
    match tape.int_in(0..=2u64, "hostile kind") {
        0 => {
            let len = tape.int_in(0..=600u64, "len") as usize;
            content(tape, len, 0)
        }
        1 => {
            let mut bytes = crate::image::png::SIGNATURE.to_vec();
            let len = tape.int_in(0..=600u64, "len") as usize;
            let mode = tape.int_in(0..=1u64, "content mode");
            bytes.extend(content(tape, len, mode));
            bytes
        }
        _ => {
            let image = draw_image(tape);
            let mut bytes = encode(&image, Global).expect("a drawn image encodes");
            for _ in 0..tape.int_in(1..=8u64, "mutations") {
                if bytes.is_empty() {
                    break;
                }
                let at = tape.int_in(0..=bytes.len() as u64 - 1, "at") as usize;
                match tape.int_in(0..=3u64, "mutation") {
                    0 => bytes[at] ^= 1 << tape.int_in(0..=7u64, "bit"),
                    1 => bytes[at] = tape.int_in(0..=255u64, "byte") as u8,
                    2 => bytes.truncate(at),
                    _ => bytes.insert(at, tape.int_in(0..=255u64, "byte") as u8),
                }
            }
            if tape.coin("fix crcs") {
                fix_crcs(&mut bytes);
            }
            bytes
        }
    }
}

#[test]
fn decode_of_any_bytes_never_panics_nor_passes_its_limit() {
    blaze_sim::check(Config::cases(1000), |tape| {
        let bytes = hostile(tape);
        let limits = Limits {
            width: tape.int_in(1..=64u64, "width limit") as u32,
            height: tape.int_in(1..=64u64, "height limit") as u32,
            bytes: tape.int_in(1..=20_000u64, "bytes limit") as usize,
        };
        let frame = Frame::with_reserve(4 * limits.bytes + (1 << 16), Tag::Other).expect("a test frame is made");
        match decode(&bytes, &frame, limits) {
            Ok(image) => {
                assert!(
                    image.width() <= limits.width && image.height() <= limits.height,
                    "decode gave {image:?}, past {limits:?}"
                );
            }
            Err(_) => {}
        }
        assert!(
            frame.peak() <= limits.bytes + ALIGN_SLACK,
            "decode of {} bytes asked its allocator for {} bytes at once, past its limit {limits:?}",
            bytes.len(),
            frame.peak()
        );
    });
}

const GREY_1X1: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3a, 0x7e, 0x9b, 0x55, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x68, 0x00, 0x00, 0x00, 0x82, 0x00, 0x81, 0xda, 0x45, 0x08, 0x3b, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// `GREY_1X1` as this encoder writes it: the same deflate bits, zlib level 0 in its header.
const ENCODED_GREY_1X1: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3a, 0x7e, 0x9b, 0x55, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x01, 0x63, 0x68, 0x00, 0x00, 0x00, 0x82, 0x00, 0x81, 0x4c, 0x17, 0xd7, 0xdf, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// 2 x 4 RGB, rows filtered Sub, Up, Average, Paeth, by Python's zlib at level 9.
const RGB_2X4: [u8; 94] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x2b, 0x8d, 0x79, 0x6e, 0x00, 0x00, 0x00, 0x25, 0x49,
    0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xe4, 0x12, 0x91, 0xdb, 0x17, 0x20, 0xc2, 0xc4, 0xc8, 0xc4, 0x6c, 0x31, 0xdb,
    0x8f, 0xf9, 0xeb, 0xaf, 0x9f, 0x3d, 0xcd, 0xdb, 0x58, 0xd8, 0xff, 0xfe, 0x62, 0x66, 0xe6, 0x01, 0x00, 0x6f, 0x8b,
    0x09, 0x4d, 0x7c, 0x76, 0x53, 0xdb, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
const RGB_2X4_PIXELS: [u8; 24] = [10, 20, 30, 200, 100, 50, 11, 22, 33, 0, 255, 128, 250, 5, 9, 9, 5, 250, 1, 2, 3, 4, 5, 6];

/// `RGB_2X4`'s stream split across three IDAT chunks of 1, 2 and 34 bytes.
const RGB_2X4_SPLIT: [u8; 118] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x2b, 0x8d, 0x79, 0x6e, 0x00, 0x00, 0x00, 0x01, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x76, 0xe6, 0x84, 0xe6, 0x00, 0x00, 0x00, 0x02, 0x49, 0x44, 0x41, 0x54, 0xda, 0x63, 0xd3,
    0x97, 0xa0, 0xcd, 0x00, 0x00, 0x00, 0x22, 0x49, 0x44, 0x41, 0x54, 0xe4, 0x12, 0x91, 0xdb, 0x17, 0x20, 0xc2, 0xc4,
    0xc8, 0xc4, 0x6c, 0x31, 0xdb, 0x8f, 0xf9, 0xeb, 0xaf, 0x9f, 0x3d, 0xcd, 0xdb, 0x58, 0xd8, 0xff, 0xfe, 0x62, 0x66,
    0xe6, 0x01, 0x00, 0x6f, 0x8b, 0x09, 0x4d, 0x4b, 0xa7, 0x40, 0x40, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

/// 3 x 2 grey with alpha at 16 bits, rows filtered Average and Paeth.
const GREY_ALPHA16_3X2: [u8; 92] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x00, 0x02, 0x10, 0x04, 0x00, 0x00, 0x00, 0x67, 0xed, 0x72, 0xd2, 0x00, 0x00, 0x00, 0x23, 0x49,
    0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x16, 0x32, 0xf9, 0xff, 0xff, 0xfb, 0x73, 0xc6, 0xc6, 0xd5, 0x67, 0x0f, 0x30,
    0xb0, 0xbc, 0x3d, 0x25, 0xa8, 0xc8, 0xc8, 0xf4, 0x81, 0x21, 0xcc, 0x94, 0x99, 0x05, 0x00, 0xa4, 0x44, 0x0a, 0x52,
    0xfb, 0xb1, 0x72, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
const GREY_ALPHA16_3X2_PIXELS: [u8; 24] = [
    0x12, 0x34, 0xff, 0xff, 0x00, 0x01, 0x80, 0x00, 0xab, 0xcd, 0x00, 0x00, 0xff, 0xfe, 0x10, 0x20, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x02, 0x03, 0x04,
];

/// A 1 x 1 grey image with Adam7 interlacing in its header.
const INTERLACED: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x01, 0x4d, 0x79, 0xab, 0xc3, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x68, 0x00, 0x00, 0x00, 0x82, 0x00, 0x81, 0x77, 0xcd, 0x72, 0xb6, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A 1 x 1 image with a palette (colour type 3).
const PALETTE: [u8; 82] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x03, 0x00, 0x00, 0x00, 0x28, 0xcb, 0x34, 0xbb, 0x00, 0x00, 0x00, 0x03, 0x50,
    0x4c, 0x54, 0x45, 0x00, 0x00, 0x00, 0xa7, 0x7a, 0x3d, 0xda, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x63, 0x60, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0x48, 0xaf, 0xa4, 0x71, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn image(width: u32, height: u32, color: Color, depth: Depth, pixels: &[u8]) -> Image<Global> {
    Image::new(width, height, color, depth, pixels.to_vec()).expect("a fixed vector's pixels are its size")
}

#[test]
fn fixed_vectors_decode_to_their_pixels() {
    let cases: [(&str, &[u8], Image<Global>); 4] = [
        ("GREY_1X1", &GREY_1X1, image(1, 1, Color::Grey, Depth::Eight, &[0x80])),
        ("RGB_2X4", &RGB_2X4, image(2, 4, Color::Rgb, Depth::Eight, &RGB_2X4_PIXELS)),
        ("RGB_2X4_SPLIT", &RGB_2X4_SPLIT, image(2, 4, Color::Rgb, Depth::Eight, &RGB_2X4_PIXELS)),
        (
            "GREY_ALPHA16_3X2",
            &GREY_ALPHA16_3X2,
            image(3, 2, Color::GreyAlpha, Depth::Sixteen, &GREY_ALPHA16_3X2_PIXELS),
        ),
    ];
    for (name, png, want) in &cases {
        let got = decode(png, Global, Limits::DEFAULT).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(got == *want, "{name} decoded to {got:?}, not {want:?}");
    }
}

#[test]
fn the_encoder_writes_known_bytes() {
    let png = encode(&image(1, 1, Color::Grey, Depth::Eight, &[0x80]), Global).expect("a 1x1 image encodes");
    assert_eq!(png[..], ENCODED_GREY_1X1[..], "a 1x1 grey 0x80 encodes to other bytes");
}

#[test]
fn refusals_name_what_they_refuse() {
    let mut bad_crc = GREY_1X1;
    bad_crc[54] ^= 1;
    assert_eq!(
        decode(&bad_crc, Global, Limits::DEFAULT).map(|_| ()),
        Err(ImageError::Crc { chunk: crate::image::ChunkName(*b"IDAT"), at: 33 })
    );
    assert_eq!(
        decode(&INTERLACED, Global, Limits::DEFAULT).map(|_| ()),
        Err(ImageError::Unsupported { what: "Adam7 interlacing", value: 1 })
    );
    assert_eq!(
        decode(&PALETTE, Global, Limits::DEFAULT).map(|_| ()),
        Err(ImageError::Unsupported { what: "colour type 3 (a palette)", value: 3 })
    );
    assert_eq!(decode(&GREY_1X1[1..], Global, Limits::DEFAULT).map(|_| ()), Err(ImageError::NotPng));
    assert_eq!(
        decode(&GREY_1X1[..45], Global, Limits::DEFAULT).map(|_| ()),
        Err(ImageError::Truncated { what: "a chunk's data and CRC", at: 33 })
    );
}

#[test]
fn the_byte_limit_is_exact() {
    let need = decode_bytes(2, 4, Color::Rgb, Depth::Eight) as usize;
    let exact = Limits { bytes: need, ..Limits::DEFAULT };
    assert!(decode(&RGB_2X4, Global, exact).is_ok(), "RGB_2X4 does not decode within exactly the {need} bytes it needs");
    let short = Limits { bytes: need - 1, ..Limits::DEFAULT };
    assert_eq!(
        decode(&RGB_2X4, Global, short).map(|_| ()),
        Err(ImageError::Limit { limit: "bytes", asked: need as u64, most: need as u64 - 1 })
    );
    let narrow = Limits { width: 1, ..Limits::DEFAULT };
    assert_eq!(
        decode(&RGB_2X4, Global, narrow).map(|_| ()),
        Err(ImageError::Limit { limit: "width", asked: 2, most: 1 })
    );
}
