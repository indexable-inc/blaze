use core::alloc::Allocator;

use blaze_own::{Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::decode::decompress_in_inner;
use crate::{
    CompressOpts, Level, ZstdDecoder, ZstdDict, ZstdEncoder, ZstdError, ZstdLimits, compress_in,
    compress_with_dict_in, decompress_in, decompress_with_dict_in, frame_content_size,
};

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_decode_in<A: Allocator>(hex: &[u8], alloc: A) -> Result<OwnVec<u8, A>, &'static str> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd hex length");
    }
    let mut out = OwnVec::with_capacity_in(hex.len() / 2, alloc);
    let mut i = 0usize;
    while i < hex.len() {
        let hi = hex_nibble(*hex.get(i).ok_or("hex")?).ok_or("bad hex")?;
        let lo = hex_nibble(*hex.get(i + 1).ok_or("hex")?).ok_or("bad hex")?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

const EMPTY_L3_CHECK_HEX: &[u8] = b"28b52ffd240001000099e9d851";
const SHORT_L3_CHECK_HEX: &[u8] =
    b"28b52ffd045889000068656c6c6f207a73746420776f726c6421a51c0cb6";
const RLE_4K_L3_CHECK_HEX: &[u8] = b"28b52ffd04584d00001041410100fbf70116c3582b6d";
const REPEAT_L1_CHECK_HEX: &[u8] = b"28b52ffd044875020082c4101a80c7190380514411b2b818bf8b133130c1e54fe394988395e809c441b9c0582efd3e4d7da3d576822b93fb25c2607180e01bad16a9a55ac65320d77790c3c051b321010048bd163f4401a167287d";
const REPEAT_L3_CHECK_HEX: &[u8] = b"28b52ffd045855020022440f17903b6300846734815aed253488991e1691229af3dc0c024064fc38b7ed9da7c8e285fe9085e7721aa82bccb9822f5871d9b1be966d6722064157c9500200eab504bf84b10905a167287d";
const REPEAT_L19_CHECK_HEX: &[u8] = b"28b52ffd04683d020022840e11907d90821ca4c389f57f2280a0ba1e0725a0a7d7b5aa793fbcd9713f3a7f9dc21f5ab6f83ca41f9ee12bda5671fdf926d5fc600a19e7d97b1e0200eab504bf84b10905a167287d";
const HUFF_PROSE_L3_CHECK_HEX: &[u8] = b"28b52ffd04583d060062cd2d24f052ac0340db7ed165d8635db77800552134675539ab2ae63322e82e8e4e914d046ca634918a9a32be56aa11d4f4994e23237cae316c3d04e4c3a383a342d9d09864605c5854504c482422201e1c1a18bea09080706050403020f0455a95b9eaf6ad464d48e5e4f0734bbee4c6a79e3bd86ef380cad675a33bcc9aecaf3f511921ca0124789bd7be919ca7aeea7687e57bb4327f6a76eab9c3acc917d8ed54b5e65db4af0ef725e8b9cda77ea7b92bbbc3c81bddc54a031007c330947b54088f4064522cc1a7b9";

const REPEAT_JSON_LINE: &[u8] =
    br#"{"id":1234,"name":"blaze-std","ok":true,"tags":["zstd","lz4","snappy"]}"#;
const PROSE_LINE: &[u8] = b"The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! Sphinx of black quartz, judge my vow. How vexingly quick daft zebras jump. 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz\n";

fn repeat_bytes<A: Allocator>(chunk: &[u8], suffix: &[u8], times: usize, alloc: A) -> OwnVec<u8, A> {
    let mut out = OwnVec::with_capacity_in((chunk.len() + suffix.len()) * times, alloc);
    for _ in 0..times {
        out.extend_from_slice(chunk);
        out.extend_from_slice(suffix);
    }
    out
}

#[test]
fn c_zstd_1_5_7_interop_vectors() {
    let h = heap();
    let alloc = h.alloc();

    // 1. empty_l3_check
    let empty_vec = hex_decode_in(EMPTY_L3_CHECK_HEX, alloc).expect("hex");
    assert_eq!(frame_content_size(empty_vec.as_slice()), Ok(Some(0)));
    let empty_out = decompress_in(empty_vec.as_slice(), ZstdLimits::SHIPPED, alloc).expect("empty");
    assert!(empty_out.is_empty());

    // 2. short_l3_check
    let short_vec = hex_decode_in(SHORT_L3_CHECK_HEX, alloc).expect("hex");
    assert_eq!(frame_content_size(short_vec.as_slice()), Ok(None));
    let short_out = decompress_in(short_vec.as_slice(), ZstdLimits::SHIPPED, alloc).expect("short");
    assert_eq!(short_out.as_slice(), b"hello zstd world!");

    // 3. rle_4k_l3_check
    let rle_vec = hex_decode_in(RLE_4K_L3_CHECK_HEX, alloc).expect("hex");
    let rle_out = decompress_in(rle_vec.as_slice(), ZstdLimits::SHIPPED, alloc).expect("rle4k");
    let expected_rle = repeat_bytes(b"A", b"", 4_096, alloc);
    assert_eq!(rle_out.as_slice(), expected_rle.as_slice());

    // 4, 5, 6. repeat_l1_check, repeat_l3_check, repeat_l19_check
    let expected_json = repeat_bytes(REPEAT_JSON_LINE, b"\n", 40, alloc);
    for hex_vec in [REPEAT_L1_CHECK_HEX, REPEAT_L3_CHECK_HEX, REPEAT_L19_CHECK_HEX] {
        let compressed = hex_decode_in(hex_vec, alloc).expect("hex");
        let decoded =
            decompress_in(compressed.as_slice(), ZstdLimits::SHIPPED, alloc).expect("repeat");
        assert_eq!(decoded.as_slice(), expected_json.as_slice());
    }

    // 7. huff_prose_l3_check
    let expected_prose = repeat_bytes(PROSE_LINE, b"", 20, alloc);
    let prose_vec = hex_decode_in(HUFF_PROSE_L3_CHECK_HEX, alloc).expect("hex");
    let prose_out = decompress_in(prose_vec.as_slice(), ZstdLimits::SHIPPED, alloc).expect("prose");
    assert_eq!(prose_out.as_slice(), expected_prose.as_slice());
}

#[test]
fn skippable_and_concatenated_frames() {
    let h = heap();
    let alloc = h.alloc();

    let f1 = compress_in(b"first frame payload ", CompressOpts::DEFAULT, alloc).expect("c1");
    let f2 = compress_in(b"second frame payload!", CompressOpts::FAST, alloc).expect("c2");

    let mut stream = OwnVec::new_in(alloc);
    // Leading skippable frame (0x184D2A50, 5 payload bytes)
    stream.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
    stream.extend_from_slice(&5u32.to_le_bytes());
    stream.extend_from_slice(b"META1");
    stream.extend_from_slice(f1.as_slice());
    // Interleaved skippable frame (0x184D2A5F, 3 payload bytes)
    stream.extend_from_slice(&0x184D_2A5Fu32.to_le_bytes());
    stream.extend_from_slice(&3u32.to_le_bytes());
    stream.extend_from_slice(b"XYZ");
    stream.extend_from_slice(f2.as_slice());

    assert_eq!(frame_content_size(stream.as_slice()), Ok(Some(20)));
    let decoded = decompress_in(stream.as_slice(), ZstdLimits::SHIPPED, alloc).expect("concat");
    assert_eq!(decoded.as_slice(), b"first frame payload second frame payload!");

    // Skippable-only stream yields empty output and Ok(Some(0)) content size.
    let mut skip_only = OwnVec::new_in(alloc);
    skip_only.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
    skip_only.extend_from_slice(&2u32.to_le_bytes());
    skip_only.extend_from_slice(b"hi");
    assert_eq!(frame_content_size(skip_only.as_slice()), Ok(Some(0)));
    let empty = decompress_in(skip_only.as_slice(), ZstdLimits::SHIPPED, alloc).expect("skip");
    assert!(empty.is_empty());

    // Enforce most_frames limit.
    let tight_frames = ZstdLimits { most_frames: 2, ..ZstdLimits::SHIPPED };
    assert_eq!(
        decompress_in(stream.as_slice(), tight_frames, alloc),
        Err(ZstdError::TooManyFrames { limit: 2 })
    );
}

#[test]
fn compression_ratio_and_multiblock_round_trip() {
    let h = heap();
    let alloc = h.alloc();

    // Repetitive JSON: must compress to < 20% of original size.
    let rep_json = repeat_bytes(REPEAT_JSON_LINE, b"\n", 100, alloc);
    for opts in [CompressOpts::DEFAULT, CompressOpts::FAST] {
        let compressed = compress_in(rep_json.as_slice(), opts, alloc).expect("compress");
        assert!(
            compressed.len() < rep_json.len() / 5,
            "expected >5x compression: {} vs {}",
            compressed.len(),
            rep_json.len()
        );
        assert_eq!(
            frame_content_size(compressed.as_slice()),
            Ok(Some(u64::try_from(rep_json.len()).unwrap_or(0)))
        );
        let decoded = decompress_in(compressed.as_slice(), ZstdLimits::SHIPPED, alloc).expect("dec");
        assert_eq!(decoded.as_slice(), rep_json.as_slice());
    }

    // Constant run uses RLE block (tiny output).
    let rle_in = repeat_bytes(b"Z", b"", 65_536, alloc);
    let rle_comp = compress_in(rle_in.as_slice(), CompressOpts::DEFAULT, alloc).expect("rle");
    assert!(rle_comp.len() < 32, "expected tiny RLE frame, got {} bytes", rle_comp.len());
    let rle_dec = decompress_in(rle_comp.as_slice(), ZstdLimits::SHIPPED, alloc).expect("rle dec");
    assert_eq!(rle_dec.as_slice(), rle_in.as_slice());

    // Multi-block input (> 128 KiB) exercises windowed frame header and cross-block offsets.
    let big = repeat_bytes(PROSE_LINE, b"", 700, alloc);
    assert!(big.len() > 131_072);
    let big_comp = compress_in(big.as_slice(), CompressOpts::DEFAULT, alloc).expect("big comp");
    assert!(big_comp.len() < big.len() / 5);
    assert_eq!(
        frame_content_size(big_comp.as_slice()),
        Ok(Some(u64::try_from(big.len()).unwrap_or(0)))
    );
    let big_dec = decompress_in(big_comp.as_slice(), ZstdLimits::SHIPPED, alloc).expect("big dec");
    assert_eq!(big_dec.as_slice(), big.as_slice());

    // UTF-8 prose with bytes > 128 exercises FSE-compressed Huffman weight descriptions.
    let utf8_line = "§4.2.1 — Huffman weights with UTF-8 symbols: αβγδ, ≤ ≥ ≠, café résumé naïve!\n".as_bytes();
    let utf8_payload = repeat_bytes(utf8_line, b"varied_token_0123456789 ", 64, alloc);
    for opts in [CompressOpts::DEFAULT, CompressOpts::FAST] {
        let comp = compress_in(utf8_payload.as_slice(), opts, alloc).expect("utf8 comp");
        assert!(comp.len() < utf8_payload.len() / 2);
        let dec = decompress_in(comp.as_slice(), ZstdLimits::SHIPPED, alloc).expect("utf8 dec");
        assert_eq!(dec.as_slice(), utf8_payload.as_slice());
    }

    // Reusable ZstdEncoder / ZstdDecoder across multiple frames & levels (Treeless + custom FSE).
    let mut enc = ZstdEncoder::try_new_in(alloc).expect("enc");
    let mut dec = ZstdDecoder::new_in(alloc);
    let mut comp_buf = OwnVec::new_in(alloc);
    let mut dec_buf = OwnVec::new_in(alloc);
    for (payload, opts) in [
        (big.as_slice(), CompressOpts::DEFAULT),
        (big.as_slice(), CompressOpts::FAST),
        (utf8_payload.as_slice(), CompressOpts::DEFAULT),
        (rep_json.as_slice(), CompressOpts::FAST),
    ] {
        comp_buf.clear();
        dec_buf.clear();
        enc.compress_into(payload, opts, &mut comp_buf).expect("reuse enc");
        dec.decompress_into(comp_buf.as_slice(), ZstdLimits::SHIPPED, &mut dec_buf)
            .expect("reuse dec");
        assert_eq!(dec_buf.as_slice(), payload);
    }
}

#[test]
fn bomb_limits_and_header_refusals() {
    let h = heap();
    let alloc = h.alloc();

    // Checksum mismatch refusal.
    let mut bad_cksum = hex_decode_in(SHORT_L3_CHECK_HEX, alloc).expect("hex");
    if let Some(last) = bad_cksum.last_mut() {
        *last ^= 0x01;
    }
    assert!(matches!(
        decompress_in(bad_cksum.as_slice(), ZstdLimits::SHIPPED, alloc),
        Err(ZstdError::ChecksumMismatch { .. })
    ));

    // Bomb limit: most_output_bytes on 4 KiB RLE vector.
    let rle_vec = hex_decode_in(RLE_4K_L3_CHECK_HEX, alloc).expect("hex");
    let small_out = ZstdLimits { most_output_bytes: 1_024, ..ZstdLimits::SHIPPED };
    assert_eq!(
        decompress_in(rle_vec.as_slice(), small_out, alloc),
        Err(ZstdError::OutputTooLarge { limit: 1_024 })
    );

    // Bomb limit: most_window_bytes (rle_vec declares a 2 MiB window).
    let small_win = ZstdLimits { most_window_bytes: 64 * 1024, ..ZstdLimits::SHIPPED };
    assert!(matches!(
        decompress_in(rle_vec.as_slice(), small_win, alloc),
        Err(ZstdError::WindowTooLarge { limit: 65_536, .. })
    ));

    // Bad magic & truncated.
    assert_eq!(
        decompress_in(b"not zstd", ZstdLimits::SHIPPED, alloc),
        Err(ZstdError::BadMagic)
    );
    assert_eq!(decompress_in(b"", ZstdLimits::SHIPPED, alloc), Err(ZstdError::Truncated));

    // Reserved bit set in FHD (bit 3 = 0x08).
    let reserved_frame = [0x28u8, 0xB5, 0x2F, 0xFD, 0x08, 0x00];
    assert_eq!(
        decompress_in(&reserved_frame, ZstdLimits::SHIPPED, alloc),
        Err(ZstdError::ReservedBitSet)
    );

    // Unsupported dictionary id (dict_id_flag = 1, dict_id = 42).
    let dict_frame = [0x28u8, 0xB5, 0x2F, 0xFD, 0x21, 42, 0x00];
    assert_eq!(
        decompress_in(&dict_frame, ZstdLimits::SHIPPED, alloc),
        Err(ZstdError::UnsupportedDictionary { dict_id: 42 })
    );
}

fn draw_payload<A: Allocator + Copy>(
    tape: &Tape,
    min_len: usize,
    max_len: usize,
    alloc: A,
) -> OwnVec<u8, A> {
    let len = tape.int_in(min_len..=max_len, "len");
    let mode = tape.int_in(0u8..=4, "mode");
    let mut data = OwnVec::with_capacity_in(len, alloc);
    match mode {
        0 => {
            // Pure random bytes
            for _ in 0..len {
                data.push(tape.int_in(0u8..=255, "b"));
            }
        }
        1 => {
            // Constant run (triggers RLE block)
            let fill = tape.int_in(0u8..=255, "fill");
            for _ in 0..len {
                data.push(fill);
            }
        }
        2 => {
            // Short periodic pattern (triggers overlapping RLE matches)
            let period = tape.int_in(1usize..=12, "period");
            let mut pat = [0u8; 12];
            for slot in pat.iter_mut().take(period) {
                *slot = tape.int_in(0u8..=255, "p");
            }
            for i in 0..len {
                data.push(pat.get(i % period).copied().unwrap_or(0));
            }
        }
        3 => {
            // Structured repetitive chunks with small mutations (triggers repeat offsets + literals)
            let chunk_len = tape.int_in(8usize..=32, "chunk");
            let mut base = [0u8; 32];
            for slot in base.iter_mut().take(chunk_len) {
                *slot = tape.int_in(b'a'..=b'z', "c");
            }
            for i in 0..len {
                let b = if i.is_multiple_of(29) {
                    tape.int_in(b'0'..=b'9', "mut")
                } else {
                    base.get(i % chunk_len).copied().unwrap_or(b'x')
                };
                data.push(b);
            }
        }
        _ => {
            // Mixed incompressible + repetitive segments
            let mut pos = 0usize;
            while pos < len {
                let seg = tape.int_in(1usize..=48, "seg").min(len - pos);
                if tape.coin("rep_seg") {
                    let b = tape.int_in(0u8..=255, "rb");
                    for _ in 0..seg {
                        data.push(b);
                    }
                } else {
                    for k in 0..seg {
                        let v = u8::try_from((pos + k * 37) % 251).unwrap_or(0);
                        data.push(v);
                    }
                }
                pos += seg;
            }
        }
    }
    data
}

/// The invariant: RFC 8878 Zstandard compression and decompression round-trip across arbitrary inputs and hostile bytes never panic.
#[test]
fn property_zstd_round_trip_and_hostile_bytes_never_panic() {
    let h = heap();
    check(Config::cases(160), |tape| {
        let alloc = h.alloc();
        let data = draw_payload(tape, 0, 2_048, alloc);
        let level = if tape.coin("fast") { Level::Fast } else { Level::Default };
        let checksum = tape.coin("checksum");
        let opts = CompressOpts { level, checksum };

        let compressed = compress_in(data.as_slice(), opts, alloc).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "compress_in failed: {e}").as_str().into())
        })?;
        let expected_fcs = u64::try_from(data.len()).unwrap_or(0);
        match frame_content_size(compressed.as_slice()) {
            Ok(Some(fcs)) if fcs == expected_fcs => {}
            other => {
                return Err(Broke::Oracle(
                    format_in!(alloc, "frame_content_size mismatch: {other:?}").as_str().into(),
                ));
            }
        }
        let decoded =
            decompress_in(compressed.as_slice(), ZstdLimits::SHIPPED, alloc).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "decompress_in failed: {e}").as_str().into())
            })?;
        if decoded.as_slice() != data.as_slice() {
            return Err(Broke::Oracle("round-trip payload mismatch".into()));
        }

        // Hostile mutated bytes must never panic (only return Ok or typed Err).
        let mut hostile = OwnVec::new_in(alloc);
        hostile.extend_from_slice(compressed.as_slice());
        let muts = tape.int_in(1usize..=8usize, "mutations");
        for _ in 0..muts {
            if !hostile.is_empty() {
                let pos = tape.index(hostile.len(), "mut_pos");
                let xor = tape.int_in(1u8..=255u8, "mut_xor");
                if let Some(slot) = hostile.get_mut(pos) {
                    *slot ^= xor;
                }
            }
        }
        let tiny_limits = ZstdLimits {
            most_output_bytes: 8_192,
            most_window_bytes: 8_192,
            most_frames: 8,
        };
        drop(decompress_in(hostile.as_slice(), tiny_limits, alloc));
        let _ = frame_content_size(hostile.as_slice());

        Ok(())
    });
}

#[test]
fn the_oracle_catches_skipped_checksum() {
    let h = heap();
    control(Config::cases(128), false, true, "", |tape, plant| {
        let alloc = h.alloc();
        let data = draw_payload(tape, 1, 512, alloc);
        let opts = CompressOpts { level: Level::Default, checksum: true };
        let mut corrupted = compress_in(data.as_slice(), opts, alloc).map_err(|e| {
            Broke::Oracle(format_in!(alloc, "compress_in failed: {e}").as_str().into())
        })?;

        // Flip a bit in the trailing 4-byte XXH64 content checksum.
        let tail_offset = tape.int_in(1usize..=4usize, "cksum_byte");
        let xor = tape.int_in(1u8..=255u8, "cksum_xor");
        let idx = corrupted.len().saturating_sub(tail_offset);
        if let Some(slot) = corrupted.get_mut(idx) {
            *slot ^= xor;
        }

        match decompress_in_inner(corrupted.as_slice(), ZstdLimits::SHIPPED, alloc, plant) {
            Err(ZstdError::ChecksumMismatch { .. }) => Ok(()),
            Ok(_) => Err(Broke::Oracle(
                "corrupted content checksum was accepted without ChecksumMismatch".into(),
            )),
            Err(other) => Err(Broke::Oracle(
                format_in!(alloc, "expected ChecksumMismatch, got {other}").as_str().into(),
            )),
        }
    });
}

#[test]
fn raw_content_dictionary_round_trips_and_improves_small_rust_files() {
    let h = heap();
    let alloc = h.alloc();
    let dict_bytes = concat!(
        "//! Standard library module.\n#![no_std]\nuse core::fmt;\nuse core::mem;\n",
        "use core::ptr;\nuse core::slice;\nuse core::marker::PhantomData;\n",
        "#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]\n",
        "#[inline]\n#[must_use]\npub const fn new() -> Self { Self }\n",
        "impl<T> Default for Option<T> { fn default() -> Self { None } }\n",
        "pub fn from_raw_parts<'a, T>(data: *const T, len: usize) -> &'a [T]\n",
        "Result<Self, Error>\nOk(Self)\nErr(Error)\n",
    )
    .as_bytes();
    let sample = concat!(
        "//! Standard library module.\n#![no_std]\nuse core::fmt;\nuse core::ptr;\n",
        "#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]\n",
        "pub struct Foo<T>(core::marker::PhantomData<T>);\n",
        "impl<T> Foo<T> { #[inline] #[must_use] pub const fn new() -> Self { Self(core::marker::PhantomData) } }\n",
    )
    .as_bytes();

    let dict = ZstdDict::try_new_in(dict_bytes, alloc).expect("dict builds");
    let mut enc = ZstdEncoder::try_new_in(alloc).expect("encoder builds");
    let mut dec = ZstdDecoder::new_in(alloc);

    for level in [Level::Fast, Level::Default] {
        let opts = CompressOpts { level, checksum: true };
        let plain = enc.compress(sample, opts).expect("plain compress");
        let with_dict = enc.compress_with_dict(sample, &dict, opts).expect("dict compress");
        assert!(
            with_dict.len() < plain.len(),
            "{level:?}: dict compressed {} bytes should beat plain {} bytes",
            with_dict.len(),
            plain.len()
        );
        let decoded = dec
            .decompress_with_dict(with_dict.as_slice(), dict.bytes(), ZstdLimits::SHIPPED)
            .expect("dict decompress");
        assert_eq!(decoded.as_slice(), sample, "{level:?}: round-trip mismatch");

        // Reusing the same decoder across multiple frames with the same dict must stay exact.
        let sample2 = b"use core::slice;\n#[inline]\n#[must_use]\npub fn len(s: &[u8]) -> usize { s.len() }\n";
        let with_dict2 = compress_with_dict_in(sample2, &dict, opts, alloc).expect("dict compress 2");
        let mut out2 = OwnVec::new_in(alloc);
        dec.decompress_with_dict_into(with_dict2.as_slice(), dict.bytes(), ZstdLimits::SHIPPED, &mut out2)
            .expect("dict decompress 2");
        assert_eq!(out2.as_slice(), sample2, "{level:?}: second frame round-trip mismatch");

        // Wrong dictionary must fail checksum (or offset) verification!
        let wrong_dict = b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let bad = decompress_with_dict_in(with_dict.as_slice(), wrong_dict, ZstdLimits::SHIPPED, alloc);
        assert!(bad.is_err(), "{level:?}: wrong dictionary must not match checksum");
    }

    // Empty input and multi-block (> 128 KiB) input with dictionary.
    let empty_c = enc.compress_with_dict(b"", &dict, CompressOpts::DEFAULT).expect("empty");
    assert_eq!(
        dec.decompress_with_dict(empty_c.as_slice(), dict.bytes(), ZstdLimits::SHIPPED)
            .expect("empty dec")
            .as_slice(),
        b""
    );
    let big = repeat_bytes(sample, b"\n", 600, alloc);
    assert!(big.len() > 150_000);
    let big_c = enc.compress_with_dict(big.as_slice(), &dict, CompressOpts::DEFAULT).expect("big");
    let big_d = dec
        .decompress_with_dict(big_c.as_slice(), dict.bytes(), ZstdLimits::SHIPPED)
        .expect("big dec");
    assert_eq!(big_d.as_slice(), big.as_slice());
}
