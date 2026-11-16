//! The crate's promises, each stated once with its oracle and a planted bug the oracle catches:
//!
//! - inflate of deflate is the input, for every level, shape and split of either side;
//! - deflate's bytes are a function of the input and the level, never of the split;
//! - both directions agree with the reference (`miniz_oxide`, a dev-dependency);
//! - a stream that would pass the caller's bounds is refused at them;
//! - each frame's checksum is checked.

use std::{alloc::Global, format, string::String, vec::Vec as StdVec};

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::{
    Adler32, Crc32, Deflate, Error, GzipDecoder, Inflate, InflateInto, Level, Limits, Tree, Why,
    ZlibDecoder, ZlibInto, adler32, crc32, gunzip, gzip, inflate, unzlib, unzlib_prefix, zlib,
    plant::{self, Plant},
};

const LOOSE: Limits = Limits { most_out: 1 << 30, most_ratio: 1 << 11, most_header: 1 << 12 };

fn empty() -> Vec<u8, Global> {
    Vec::new_in(Global)
}

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

/// `data` deflated, written in pieces of the sizes `splits` gives (the rest in one).
fn compress(data: &[u8], level: Level, splits: &[usize]) -> Vec<u8, Global> {
    let mut d = Deflate::new_in(level, Global);
    let mut out = empty();
    let mut at = 0;
    for &size in splits {
        let end = (at + size).min(data.len());
        d.write(&data[at..end], &mut out).expect("deflate writes");
        at = end;
    }
    d.write(&data[at..], &mut out).expect("deflate writes");
    d.finish(&mut out).expect("deflate finishes");
    out
}

/// `stream` inflated, fed in pieces of the sizes `splits` gives.
fn decompress(stream: &[u8], limits: Limits, splits: &[usize]) -> Result<Vec<u8, Global>, Error> {
    let mut i = Inflate::new_in(limits, Global);
    let mut out = empty();
    let mut at = 0usize;
    let mut sizes = splits.iter().copied().chain(core::iter::repeat(usize::MAX));
    while at < stream.len() && !i.is_done() {
        let end = at.saturating_add(sizes.next().unwrap_or(usize::MAX).max(1)).min(stream.len());
        let fed = i.feed(&stream[at..end], &mut out)?;
        // Until the stream ends, a feed takes all it is given (into the output or the bits).
        assert!(fed.done || fed.consumed == end - at, "a feed left input before the stream's end");
        at += fed.consumed;
    }
    i.finish()?;
    Ok(out)
}

/// `stream` inflated into `out` with [`InflateInto`], fed in pieces; the bytes written.
fn decompress_into(stream: &[u8], out: &mut [u8], splits: &[usize]) -> Result<usize, Error> {
    let mut i = InflateInto::new(LOOSE);
    let mut at = 0usize;
    let mut sizes = splits.iter().copied().chain(core::iter::repeat(usize::MAX));
    while at < stream.len() && !i.is_done() {
        let end = at.saturating_add(sizes.next().unwrap_or(usize::MAX).max(1)).min(stream.len());
        at += i.feed(&stream[at..end], out)?.consumed;
    }
    i.finish()?;
    Ok(i.written() as usize)
}

/// Input of a shape that tests one edge of the matcher.
fn shaped(tape: &Tape) -> StdVec<u8> {
    let len = tape.int_in(0usize..=200_000, "len");
    match tape.below(6, "shape") {
        // Random bytes: incompressible, so blocks are stored.
        0 => tape.bytes(0..=len.min(70_000), "random"),
        // All one byte: matches of 258 at distance 1, overlapping their own output.
        1 => {
            let byte = tape.int_in(0u8..=255, "byte");
            std::vec![byte; len]
        }
        // A short period: overlapping matches of every distance below it.
        2 => {
            let period = tape.bytes(1..=40, "period");
            (0..len).map(|i| period[i % period.len()]).collect()
        }
        // A piece repeated at the window's edge: 32768 back is a match, 32769 is not.
        3 => {
            let piece = tape.bytes(3..=600, "piece");
            let gap = tape.int_in(32_760usize..=32_775, "gap");
            let mut data = tape.bytes(gap..=gap, "filler");
            data.splice(0..0, piece.iter().copied());
            data.extend_from_slice(&piece);
            data
        }
        // Text-like: few symbols, uneven counts, many short matches.
        4 => {
            let words: [&[u8]; 8] = [b"the ", b"crate ", b"blaze ", b"fn ", b"{\n", b"}\n", b"let x = ", b"0;\n"];
            let mut data = StdVec::new();
            while data.len() < len.min(60_000) {
                data.extend_from_slice(words[tape.index(words.len(), "word")]);
            }
            data
        }
        // A mix: runs, then random, then a copy of an earlier run: blocks of each kind.
        _ => {
            let mut data = std::vec![7u8; len.min(30_000)];
            data.extend(tape.bytes(0..=5_000, "noise"));
            let copy: StdVec<u8> = data[..data.len().min(1_000)].to_vec();
            data.extend_from_slice(&copy);
            data
        }
    }
}

fn level(tape: &Tape) -> Level {
    *tape.pick(&[Level::Store, Level::Fast, Level::Default], "level").expect("three levels")
}

fn splits(tape: &Tape, label: &'static str) -> StdVec<usize> {
    tape.vec(0..=6, label, |t| t.int_in(0usize..=70_000, label))
}

/// The round trip's oracle.
fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let data = shaped(tape);
    let level = level(tape);
    let whole = compress(&data, level, &[]);
    let split = compress(&data, level, &splits(tape, "write splits"));
    if whole[..] != split[..] {
        return Err(oracle(format!("deflate's bytes depend on the split ({} vs {} bytes)", whole.len(), split.len())));
    }
    let back = decompress(&whole, LOOSE, &splits(tape, "read splits"))
        .map_err(|e| oracle(format!("round trip refused: {e}")))?;
    if back[..] != data[..] {
        return Err(oracle(format!("round trip differs: {} bytes in, {} back", data.len(), back.len())));
    }
    let mut flat = std::vec![0u8; data.len()];
    let n = decompress_into(&whole, &mut flat, &splits(tape, "slice splits"))
        .map_err(|e| oracle(format!("the slice inflater refused: {e}")))?;
    if n != data.len() || flat != data {
        return Err(oracle(format!("the slice inflater differs: {} bytes in, {n} back", data.len())));
    }
    if !data.is_empty() {
        let mut short = std::vec![0u8; data.len() - 1];
        match decompress_into(&whole, &mut short, &[]) {
            Err(e) if matches!(e.why, Why::OutputOver { .. }) => {}
            other => return Err(oracle(format!("a slice one byte short gave {other:?}, not OutputOver"))),
        }
    }
    let theirs = miniz_oxide::inflate::decompress_to_vec(&whole)
        .map_err(|e| oracle(format!("the reference refuses our stream: {e:?}")))?;
    if theirs != data {
        return Err(oracle(String::from("the reference reads our stream differently")));
    }
    let reference = miniz_oxide::deflate::compress_to_vec(&data, 6);
    let ours = decompress(&reference, LOOSE, &splits(tape, "reference splits"))
        .map_err(|e| oracle(format!("we refuse the reference's stream: {e}")))?;
    if ours[..] != data[..] {
        return Err(oracle(String::from("we read the reference's stream differently")));
    }
    Ok(())
}

#[test]
fn inflate_of_deflate_is_the_input() {
    check(Config::cases(96), round_trip);
}

#[test]
fn the_oracle_catches_a_distance_off_by_one() {
    control(Config::cases(96), Plant::Clean, Plant::DistanceOffByOne, "round trip", |tape, p| {
        plant::with(p, || round_trip(tape))
    });
}

#[test]
fn known_answers_from_rfc_1951() {
    let read = |bytes: &[u8]| decompress(bytes, LOOSE, &[]);
    // A final stored block of five bytes.
    assert_eq!(&read(&[0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o']).expect("stored")[..], b"hello");
    // A final fixed block: the literal 'a' (8 bits, 0x30 + 0x61) then the end (7 zero bits).
    assert_eq!(&read(&[0x4b, 0x04, 0x00]).expect("fixed")[..], b"a");
    // An empty final fixed block: 1, 01, then seven zero bits.
    assert_eq!(read(&[0x03, 0x00]).expect("empty").len(), 0);
    // Block type 3.
    assert_eq!(read(&[0x07]).map_err(|e| e.why), Err(Why::BlockType));
    // A stored length whose complement is wrong.
    assert_eq!(
        read(&[0x01, 0x05, 0x00, 0x00, 0x00]).map_err(|e| e.why),
        Err(Why::StoredLength { len: 5, nlen: 0 })
    );
    // A fixed block whose first symbol is a match of 3 at distance 1: nothing to copy.
    assert_eq!(read(&[0x03, 0x02]).map_err(|e| e.why), Err(Why::TooFar { distance: 1, have: 0 }));
    // The 'a' block cut short.
    assert_eq!(read(&[0x4b, 0x04]).map_err(|e| e.why), Err(Why::Truncated));
    // A dynamic block whose code-length code is one code of length 1: incomplete.
    // Header 1,10; HLIT 0, HDIST 0, HCLEN 0 (four lengths: 16, 17, 18, 0 -> 1, 0, 0, 0).
    let bits: &[(u32, u32)] = &[(1, 1), (2, 2), (0, 5), (0, 5), (0, 4), (1, 3), (0, 3), (0, 3), (0, 3)];
    assert_eq!(read(&pack_bits(bits)).map_err(|e| e.why), Err(Why::Incomplete(Tree::Precode)));
}

/// `(value, bits)` pairs packed lowest bit first, as DEFLATE sends its fields.
fn pack_bits(fields: &[(u32, u32)]) -> StdVec<u8> {
    let (mut out, mut acc, mut n) = (StdVec::new(), 0u64, 0u32);
    for &(value, bits) in fields {
        acc |= u64::from(value) << n;
        n += bits;
        while n >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            n -= 8;
        }
    }
    if n > 0 {
        out.push(acc as u8);
    }
    out
}

#[test]
fn checksums_known_answers() {
    assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    assert_eq!(adler32(b""), 1);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b""), 0);
    let data: StdVec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 13) as u8).collect();
    let (mut a, mut c) = (Adler32::new(), Crc32::new());
    for piece in data.chunks(997) {
        a.update(piece);
        c.update(piece);
    }
    assert_eq!(a.finish(), adler32(&data));
    assert_eq!(c.finish(), crc32(&data));
}

/// A bomb: a long run of zeros, read with a ratio bound below its own.
fn bomb(tape: &Tape) -> Result<(), Broke> {
    let len = tape.int_in(1usize << 20..=4 << 20, "bomb size");
    let stream = compress(&std::vec![0u8; len], Level::Default, &[]);
    let limits = Limits { most_out: 1 << 30, most_ratio: 64, most_header: 0 };
    match decompress(&stream, limits, &splits(tape, "bomb splits")) {
        Err(Error { why: Why::RatioOver { output, input, most_ratio: 64 }, .. })
            if output <= input * 64 + crate::RATIO_SLACK + 258 =>
        {
            Ok(())
        }
        Err(e) => Err(oracle(format!("the bomb was refused for another reason: {e}"))),
        Ok(out) => Err(oracle(format!("bomb not refused: {} bytes out of {}", out.len(), stream.len()))),
    }
}

#[test]
fn a_bomb_is_refused_at_the_ratio() {
    check(Config::cases(8), bomb);
}

#[test]
fn the_oracle_catches_an_unchecked_ratio() {
    control(Config::cases(8), Plant::Clean, Plant::RatioUnchecked, "bomb not refused", |tape, p| {
        plant::with(p, || bomb(tape))
    });
}

#[test]
fn output_past_the_most_is_refused() {
    let stream = compress(&[9u8; 100_000], Level::Fast, &[]);
    let limits = Limits { most_out: 50_000, most_ratio: 1 << 20, most_header: 0 };
    let mut out = empty();
    let got = inflate(&stream, limits, Global, &mut out).map_err(|e| e.why);
    assert_eq!(got, Err(Why::OutputOver { most: 50_000 }));
    assert!(out.len() <= 50_000, "{} bytes written past the bound", out.len());
}

#[test]
fn zlib_round_trip_and_prefix() {
    check(Config::cases(32), |tape| {
        let data = shaped(tape);
        let level = level(tape);
        let mut first = empty();
        zlib(&data, level, Global, &mut first).map_err(|e| Broke::Harness(format!("{e}")))?;
        let theirs = miniz_oxide::inflate::decompress_to_vec_zlib(&first)
            .map_err(|e| oracle(format!("the reference refuses our zlib: {e:?}")))?;
        if theirs != data {
            return Err(oracle(String::from("the reference reads our zlib differently")));
        }
        // Two streams end to end, as a git pack has them: the first's length is found.
        let mut both = Vec::from_slice_in(&first, Global);
        zlib(b"second", Level::Fast, Global, &mut both).map_err(|e| Broke::Harness(format!("{e}")))?;
        let mut out = empty();
        let len = unzlib_prefix(&both, LOOSE, Global, &mut out).map_err(|e| oracle(format!("{e}")))?;
        if len != first.len() || out[..] != data[..] {
            return Err(oracle(format!("the prefix is {len} bytes, not {}", first.len())));
        }
        let mut second = empty();
        unzlib(&both[len..], LOOSE, Global, &mut second).map_err(|e| oracle(format!("{e}")))?;
        if &second[..] != b"second" {
            return Err(oracle(String::from("the second stream reads wrong")));
        }
        // Fed a byte at a time, the same.
        let mut z = ZlibDecoder::new_in(LOOSE, Global);
        let mut piecewise = empty();
        for byte in first.iter() {
            z.feed(core::slice::from_ref(byte), &mut piecewise).map_err(|e| oracle(format!("{e}")))?;
        }
        z.finish().map_err(|e| oracle(format!("{e}")))?;
        if piecewise[..] != data[..] {
            return Err(oracle(String::from("zlib fed bytewise differs")));
        }
        // Into a slice, a byte at a time, the same; the bytes after it stay its next thing's.
        let mut z = ZlibInto::new(LOOSE);
        let mut flat = std::vec![0u8; data.len()];
        let mut at = 0;
        while !z.finish().is_ok() && at < both.len() {
            at += z.feed(&both[at..at + 1], &mut flat).map_err(|e| oracle(format!("{e}")))?.consumed;
        }
        z.finish().map_err(|e| oracle(format!("{e}")))?;
        if flat != data || z.written() as usize != data.len() || z.stream_len() as usize != first.len() {
            return Err(oracle(String::from("zlib into a slice differs")));
        }
        Ok(())
    });
}

#[test]
fn zlib_checks_its_adler32() {
    let mut stream = empty();
    zlib(b"some bytes to check", Level::Default, Global, &mut stream).expect("zlib");
    let last = stream.len() - 1;
    stream[last] ^= 1;
    let got = unzlib(&stream, LOOSE, Global, &mut empty()).map_err(|e| e.why);
    assert!(matches!(got, Err(Why::Adler32 { .. })), "{got:?}");
    assert_eq!(
        unzlib(&[0x78, 0x9d, 0x03, 0x00], LOOSE, Global, &mut empty()).map_err(|e| e.why),
        Err(Why::ZlibHeader { cmf: 0x78, flg: 0x9d })
    );
}

/// gzip: a member written here or by hand, read back in pieces, its CRC checked.
fn gzip_case(tape: &Tape) -> Result<(), Broke> {
    let data = shaped(tape);
    let mut member = empty();
    gzip(&data, level(tape), Global, &mut member).map_err(|e| Broke::Harness(format!("{e}")))?;
    let corrupt = tape.coin("corrupt the CRC");
    if corrupt {
        let at = member.len() - 8 + tape.index(4, "CRC byte");
        member[at] ^= 1 << tape.below(8, "bit");
    }
    let mut g = GzipDecoder::new_in(LOOSE, Global);
    let mut out = empty();
    let mut at = 0usize;
    let mut result = Ok(());
    for size in splits(tape, "gzip splits").into_iter().chain(core::iter::once(usize::MAX)) {
        let end = at.saturating_add(size.max(1)).min(member.len());
        match g.feed(&member[at..end], &mut out) {
            Ok(fed) => at += fed.consumed,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    let result = result.and_then(|()| g.finish());
    match (corrupt, result) {
        (false, Ok(())) if out[..] == data[..] => Ok(()),
        (false, Ok(())) => Err(oracle(String::from("gzip round trip differs"))),
        (false, Err(e)) => Err(oracle(format!("gzip round trip refused: {e}"))),
        (true, Err(Error { why: Why::Crc32 { .. }, .. })) => Ok(()),
        (true, other) => Err(oracle(format!("corrupt CRC not refused: {other:?}"))),
    }
}

#[test]
fn gzip_round_trip_and_crc() {
    check(Config::cases(48), gzip_case);
}

#[test]
fn the_oracle_catches_an_unchecked_gzip_crc() {
    control(Config::cases(48), Plant::Clean, Plant::GzipCrcUnchecked, "corrupt CRC not refused", |tape, p| {
        plant::with(p, || gzip_case(tape))
    });
}

#[test]
fn gzip_header_fields_and_bounds() {
    // A member with every optional field, as other writers make it.
    let mut body = empty();
    crate::deflate(b"payload", Level::Fast, Global, &mut body).expect("deflate");
    let mut member = StdVec::from([0x1f, 0x8b, 8, 0x02 | 0x04 | 0x08 | 0x10, 1, 2, 3, 4, 0, 3]);
    member.extend_from_slice(&[3, 0, b'x', b'y', b'z']);
    member.extend_from_slice(b"name.tar\0a comment\0");
    let hcrc = (crc32(&member) & 0xffff) as u16;
    member.extend_from_slice(&hcrc.to_le_bytes());
    member.extend_from_slice(&body);
    member.extend_from_slice(&crc32(b"payload").to_le_bytes());
    member.extend_from_slice(&7u32.to_le_bytes());
    let mut out = empty();
    gunzip(&member, LOOSE, Global, &mut out).expect("a member with every field");
    assert_eq!(&out[..], b"payload");

    // The header bound: a name with no end is refused at it, not buffered.
    let mut endless = StdVec::from([0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 3]);
    endless.extend(core::iter::repeat_n(b'n', 10_000));
    let limits = Limits { most_header: 100, ..LOOSE };
    let got = gunzip(&endless, limits, Global, &mut empty()).map_err(|e| e.why);
    assert_eq!(got, Err(Why::GzipHeaderOver { most: 100 }));

    // A wrong header CRC, a wrong magic, a reserved flag.
    let at = member.len() - body.len() - 9;
    member[at] ^= 0xff;
    assert!(matches!(gunzip(&member, LOOSE, Global, &mut empty()).map_err(|e| e.why), Err(Why::GzipHeaderCrc { .. })));
    assert_eq!(gunzip(&[0x1f, 0x8c], LOOSE, Global, &mut empty()).map_err(|e| e.why), Err(Why::GzipMagic { id1: 0x1f, id2: 0x8c }));
    assert_eq!(gunzip(&[0x1f, 0x8b, 8, 0x20], LOOSE, Global, &mut empty()).map_err(|e| e.why), Err(Why::GzipFlags(0x20)));
}

#[test]
fn deflate_is_deterministic_across_runs() {
    // Equal input and level, equal bytes: the record keys of compressed records rest on it.
    let data: StdVec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(i) >> 7) as u8).collect();
    for level in [Level::Store, Level::Fast, Level::Default] {
        let a = compress(&data, level, &[]);
        let b = compress(&data, level, &[1, 2, 3, 65_536, 100_000]);
        assert!(a[..] == b[..], "{level:?}: bytes differ by split");
    }
}

/// Each vector routine equals its scalar twin on random input at every alignment.
#[test]
fn vector_routines_equal_their_scalar_twins() {
    use crate::simd::{adler_run, adler_run_scalar, match_len, match_len_scalar};
    check(Config::cases(400), |tape| {
        // Two copies of a piece with a few bytes changed: long agreements that end anywhere.
        let piece = tape.bytes(0..=600, "piece");
        let skew = tape.int_in(0usize..=31, "skew");
        let mut data = tape.bytes(skew..=skew, "lead");
        let a = data.len();
        data.extend_from_slice(&piece);
        let b = data.len() + tape.int_in(0usize..=17, "gap");
        data.resize(b, 0);
        data.extend_from_slice(&piece);
        for _ in 0..tape.int_in(0usize..=3, "changes") {
            if !piece.is_empty() {
                let at = b + tape.index(piece.len(), "changed at");
                data[at] = data[at].wrapping_add(1 + tape.int_in(0u8..=254, "by"));
            }
        }
        let most = piece.len();
        let from = tape.int_in(0usize..=most.min(3), "from");
        let (v, s) = (match_len(&data, a, b, from, most), match_len_scalar(&data, a, b, from, most));
        if v != s {
            return Err(oracle(format!("match_len {v}, its twin {s} (a {a}, b {b}, from {from}, most {most})")));
        }
        let run = &data[skew..data.len().min(skew + 5_552)];
        let start = (tape.int_in(1u32..=65_520, "a"), tape.int_in(0u32..=65_520, "b"));
        let (v, s) = (adler_run(start.0, start.1, run), adler_run_scalar(start.0, start.1, run));
        if v != s {
            return Err(oracle(format!("adler_run {v:?}, its twin {s:?} over {} bytes", run.len())));
        }
        Ok(())
    });
}
