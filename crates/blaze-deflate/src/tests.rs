//! The crate's promises, each stated once with its oracle and a planted bug the oracle catches:
//!
//! - inflate of deflate is the input, for every level, shape and split of either side;
//! - deflate's bytes are a function of the input and the level, never of the split;
//! - both directions agree with the reference (`miniz_oxide`, a dev-dependency);
//! - a stream that would pass the caller's bounds is refused at them, and only then: the bounds
//!   are drawn down to their least legal values (D165) and each refusal is reached;
//! - each frame's checksum is checked.

use core::fmt;
use std::{format, string::String, vec::Vec as StdVec};

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls, points};

use crate::{
    Adler32, Crc32, DeflatePlant, Error, Fed, Level, Limits, MAX_RATIO, PlantedDeflate, PlantedGzipDecoder,
    PlantedInflate, PlantedInflateInto, RATIO_SLACK, Tree, Why, ZlibDecoder, ZlibInto, adler32, crc32, gunzip,
    gzip, inflate, unzlib, unzlib_prefix, zlib,
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const LOOSE: Limits = Limits { most_out: 1 << 30, most_ratio: 1 << 11, most_header: 1 << 12 };

fn empty() -> Vec<u8, Alloc<'static>> {
    Vec::new_in(heap())
}

/// `data` deflated, written in pieces of the sizes `splits` gives (the rest in one).
fn compress(data: &[u8], level: Level, splits: &[usize], plant: impl Planted<DeflatePlant>) -> Vec<u8, Alloc<'static>> {
    let mut d = PlantedDeflate::new_planted(level, heap(), plant);
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
fn decompress(
    stream: &[u8],
    limits: Limits,
    splits: &[usize],
    plant: impl Planted<DeflatePlant>,
) -> Result<Vec<u8, Alloc<'static>>, Error> {
    let mut i = PlantedInflate::new_planted(limits, heap(), plant);
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

/// `stream` inflated into `out` with [`crate::InflateInto`], fed in pieces; the bytes written.
fn decompress_into(stream: &[u8], out: &mut [u8], splits: &[usize]) -> Result<usize, Error> {
    let mut i = crate::InflateInto::new(LOOSE);
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

/// The round trip's invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TripBroken {
    /// deflate wrote other bytes for the same input split another way.
    SplitDepends { whole: usize, split: usize },
    /// Our inflater refused our own stream.
    Refused { why: Why },
    /// Our inflater read our stream as other bytes.
    Differs { len: usize, back: usize },
    /// The slice inflater refused our stream.
    SliceRefused { why: Why },
    /// The slice inflater read our stream as other bytes.
    SliceDiffers { len: usize, back: usize },
    /// A slice one byte short of the output was not refused as `OutputOver`.
    ShortNotRefused { got: String },
    /// The reference refused our stream.
    ReferenceRefuses { error: String },
    /// The reference read our stream as other bytes.
    ReferenceDiffers,
    /// We refused the reference's stream.
    TheirsRefused { why: Why },
    /// We read the reference's stream as other bytes.
    TheirsDiffers,
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::SplitDepends { whole, split } => {
                write!(f, "deflate's bytes depend on the split ({whole} vs {split} bytes)")
            }
            TripBroken::Refused { why } => write!(f, "round trip refused: {why}"),
            TripBroken::Differs { len, back } => write!(f, "round trip differs: {len} bytes in, {back} back"),
            TripBroken::SliceRefused { why } => write!(f, "the slice inflater refused: {why}"),
            TripBroken::SliceDiffers { len, back } => {
                write!(f, "the slice inflater differs: {len} bytes in, {back} back")
            }
            TripBroken::ShortNotRefused { got } => write!(f, "a slice one byte short gave {got}, not OutputOver"),
            TripBroken::ReferenceRefuses { error } => write!(f, "the reference refuses our stream: {error}"),
            TripBroken::ReferenceDiffers => write!(f, "the reference reads our stream differently"),
            TripBroken::TheirsRefused { why } => write!(f, "we refuse the reference's stream: {why}"),
            TripBroken::TheirsDiffers => write!(f, "we read the reference's stream differently"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the round trip, each with the invariant that must catch it.
    enum TripPlant breaks TripBroken {
        /// The deflater as it is.
        None,
        /// Every match one byte farther back: the inflater reads other bytes, or, where the match
        /// reached the stream's first byte, refuses the distance as too far.
        DistanceOffByOne => TripBroken::Differs { .. } | TripBroken::Refused { why: Why::TooFar { .. } },
    }
}

impl TripPlant {
    /// The codec's plant this control plants.
    fn planted(self) -> DeflatePlant {
        match self {
            TripPlant::None => DeflatePlant::None,
            TripPlant::DistanceOffByOne => DeflatePlant::DistanceOffByOne,
        }
    }
}

/// The round trip's oracle.
fn round_trip(tape: &Tape, plant: impl Planted<DeflatePlant>) -> Result<(), Broken<TripBroken>> {
    let data = shaped(tape);
    let level = level(tape);
    let whole = compress(&data, level, &[], plant);
    let split = compress(&data, level, &splits(tape, "write splits"), plant);
    if whole[..] != split[..] {
        return Err(Broken::Oracle(TripBroken::SplitDepends { whole: whole.len(), split: split.len() }));
    }
    let back = decompress(&whole, LOOSE, &splits(tape, "read splits"), NoPlant)
        .map_err(|e| Broken::Oracle(TripBroken::Refused { why: e.why }))?;
    if back[..] != data[..] {
        return Err(Broken::Oracle(TripBroken::Differs { len: data.len(), back: back.len() }));
    }
    let mut flat = std::vec![0u8; data.len()];
    let n = decompress_into(&whole, &mut flat, &splits(tape, "slice splits"))
        .map_err(|e| Broken::Oracle(TripBroken::SliceRefused { why: e.why }))?;
    if n != data.len() || flat != data {
        return Err(Broken::Oracle(TripBroken::SliceDiffers { len: data.len(), back: n }));
    }
    if !data.is_empty() {
        let mut short = std::vec![0u8; data.len() - 1];
        match decompress_into(&whole, &mut short, &[]) {
            Err(e) if matches!(e.why, Why::OutputOver { .. }) => {}
            Err(e) => return Err(Broken::Oracle(TripBroken::ShortNotRefused { got: format!("{e}") })),
            Ok(n) => return Err(Broken::Oracle(TripBroken::ShortNotRefused { got: format!("{n} bytes") })),
        }
    }
    let theirs = miniz_oxide::inflate::decompress_to_vec(&whole)
        .map_err(|e| Broken::Oracle(TripBroken::ReferenceRefuses { error: format!("{e:?}") }))?;
    if theirs != data {
        return Err(Broken::Oracle(TripBroken::ReferenceDiffers));
    }
    let reference = miniz_oxide::deflate::compress_to_vec(&data, 6);
    let ours = decompress(&reference, LOOSE, &splits(tape, "reference splits"), NoPlant)
        .map_err(|e| Broken::Oracle(TripBroken::TheirsRefused { why: e.why }))?;
    if ours[..] != data[..] {
        return Err(Broken::Oracle(TripBroken::TheirsDiffers));
    }
    Ok(())
}

#[test]
fn inflate_of_deflate_is_the_input() {
    check(Config::cases(96), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_round_trip_plant_is_caught_by_its_invariant() {
    controls(Config::cases(96), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

#[test]
fn known_answers_from_rfc_1951() {
    let read = |bytes: &[u8]| decompress(bytes, LOOSE, &[], NoPlant);
    // A final stored block of five bytes.
    assert_eq!(&read(&[0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o']).expect("stored")[..], b"hello");
    // A final fixed block: the literal 'a' (8 bits, 0x30 + 0x61) then the end (7 zero bits).
    assert_eq!(&read(&[0x4b, 0x04, 0x00]).expect("fixed")[..], b"a");
    // An empty final fixed block: 1, 01, then seven zero bits.
    assert_eq!(read(&[0x03, 0x00]).expect("empty").len(), 0);
    // Block type 3.
    assert_eq!(read(&[0x07]).map_err(|e| e.why).map(|_| ()), Err(Why::BlockType));
    // A stored length whose complement is wrong.
    assert_eq!(
        read(&[0x01, 0x05, 0x00, 0x00, 0x00]).map_err(|e| e.why).map(|_| ()),
        Err(Why::StoredLength { len: 5, nlen: 0 })
    );
    // A fixed block whose first symbol is a match of 3 at distance 1: nothing to copy.
    assert_eq!(read(&[0x03, 0x02]).map_err(|e| e.why).map(|_| ()), Err(Why::TooFar { distance: 1, have: 0 }));
    // The 'a' block cut short.
    assert_eq!(read(&[0x4b, 0x04]).map_err(|e| e.why).map(|_| ()), Err(Why::Truncated));
    // A dynamic block whose code-length code is one code of length 1: incomplete.
    // Header 1,10; HLIT 0, HDIST 0, HCLEN 0 (four lengths: 16, 17, 18, 0 -> 1, 0, 0, 0).
    let bits: &[(u32, u32)] = &[(1, 1), (2, 2), (0, 5), (0, 5), (0, 4), (1, 3), (0, 3), (0, 3), (0, 3)];
    assert_eq!(read(&pack_bits(bits)).map_err(|e| e.why).map(|_| ()), Err(Why::Incomplete(Tree::Precode)));
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

/// The bomb's invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum BombBroken {
    /// A run of zeros past the ratio was read to its end.
    NotRefused { out: usize, stream: usize },
    /// The bomb was refused, but not as `RatioOver` at its ratio.
    OtherRefusal { why: Why },
}

impl fmt::Display for BombBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BombBroken::NotRefused { out, stream } => write!(f, "bomb not refused: {out} bytes out of {stream}"),
            BombBroken::OtherRefusal { why } => write!(f, "the bomb was refused for another reason: {why}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the bomb bound.
    enum BombPlant breaks BombBroken {
        /// The inflater as it is.
        None,
        /// The ratio is never checked: the bomb is read whole.
        RatioUnchecked => BombBroken::NotRefused { .. },
    }
}

impl BombPlant {
    /// The codec's plant this control plants.
    fn planted(self) -> DeflatePlant {
        match self {
            BombPlant::None => DeflatePlant::None,
            BombPlant::RatioUnchecked => DeflatePlant::RatioUnchecked,
        }
    }
}

/// A bomb: a long run of zeros, read with a ratio bound below its own.
fn bomb(tape: &Tape, plant: impl Planted<DeflatePlant>) -> Result<(), Broken<BombBroken>> {
    let len = tape.int_in(1usize << 20..=4 << 20, "bomb size");
    let stream = compress(&std::vec![0u8; len], Level::Default, &[], NoPlant);
    let limits = Limits { most_out: 1 << 30, most_ratio: 64, most_header: 0 };
    match decompress(&stream, limits, &splits(tape, "bomb splits"), plant) {
        Err(Error { why: Why::RatioOver { output, input, most_ratio: 64 }, .. })
            if output <= input * 64 + RATIO_SLACK + 258 =>
        {
            Ok(())
        }
        Err(e) => Err(Broken::Oracle(BombBroken::OtherRefusal { why: e.why })),
        Ok(out) => Err(Broken::Oracle(BombBroken::NotRefused { out: out.len(), stream: stream.len() })),
    }
}

#[test]
fn a_bomb_is_refused_at_the_ratio() {
    check(Config::cases(8), |tape| bomb(tape, NoPlant));
}

#[test]
fn every_bomb_plant_is_caught_by_its_invariant() {
    controls(Config::cases(8), |tape, plant: BombPlant| bomb(tape, plant.planted()));
}

#[test]
fn output_past_the_most_is_refused() {
    let stream = compress(&[9u8; 100_000], Level::Fast, &[], NoPlant);
    let limits = Limits { most_out: 50_000, most_ratio: 1 << 20, most_header: 0 };
    let mut out = empty();
    let got = inflate(&stream, limits, heap(), &mut out).map_err(|e| e.why);
    assert_eq!(got, Err(Why::OutputOver { most: 50_000 }));
    assert!(out.len() <= 50_000, "{} bytes written past the bound", out.len());
}

/// The bounds' invariants: a reader refuses at a bound only when the stream passes it, and
/// never reads past one.
#[derive(Clone, PartialEq, Eq, Debug)]
enum BoundBroken {
    /// A member that passes a bound was read whole.
    Passed { bound: &'static str, limits: Limits, out: usize, body: usize, name: Option<usize> },
    /// A member was refused at a bound it does not pass.
    RefusedEarly { why: Why, limits: Limits, out: usize, body: usize, name: Option<usize> },
    /// A member within its bounds was refused for another reason.
    Refused { why: Why },
    /// A member was read as other bytes.
    Differs { len: usize, back: usize },
}

impl fmt::Display for BoundBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundBroken::Passed { bound, limits, out, body, name } => write!(
                f,
                "a member past its {bound} was read: {out} bytes from a {body}-byte body, name {name:?}, \
                 limits {limits:?}"
            ),
            BoundBroken::RefusedEarly { why, limits, out, body, name } => write!(
                f,
                "a member refused ({why}) within its bounds: {out} bytes from a {body}-byte body, name \
                 {name:?}, limits {limits:?}"
            ),
            BoundBroken::Refused { why } => write!(f, "a member was refused: {why}"),
            BoundBroken::Differs { len, back } => write!(f, "a member of {len} bytes read back as {back}"),
        }
    }
}

impl blaze_sim::Limits for Limits {
    /// The bounds the crate's tests read with; the crate itself ships none (every field is its
    /// caller's choice).
    const SHIPPED: Limits = LOOSE;

    fn draw(tape: &Tape) -> Limits {
        Limits {
            most_out: tape.limit(LOOSE.most_out, 0, "most out"),
            most_ratio: tape.limit(LOOSE.most_ratio, 0, "most ratio"),
            most_header: tape.limit(LOOSE.most_header, 0, "most header"),
        }
    }
}

points! {
    /// Where the bounds' simulation has to get: each bound refusing a member.
    enum BoundPoint {
        /// `Limits::most_out` refused the output.
        OutputOver,
        /// `Limits::most_ratio` refused the output.
        RatioOver,
        /// `Limits::most_header` refused a gzip header.
        GzipHeaderOver,
    }
}

/// One member, written here with a name of a drawn length or none, read under drawn bounds.
fn bounds(tape: &Tape) -> Result<(), Broken<BoundBroken>> {
    let limits: Limits = tape.limits();
    let data = match tape.coin("zeros or random") {
        true => std::vec![0u8; tape.int_in(0usize..=20_000, "zeros")],
        false => tape.bytes(0..=2_000, "random"),
    };
    let name = match tape.coin("named") {
        true => Some(tape.int_in(0usize..=40, "name length")),
        false => None,
    };
    let mut body = empty();
    crate::deflate(&data, level(tape), heap(), &mut body).map_err(|e| Broken::Harness(format!("{e}")))?;
    let flags = match name {
        Some(_) => 0x08,
        None => 0,
    };
    let mut member = StdVec::from([0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 3]);
    if let Some(len) = name {
        member.extend(core::iter::repeat_n(b'n', len));
        member.push(0);
    }
    member.extend_from_slice(&body);
    member.extend_from_slice(&crc32(&data).to_le_bytes());
    member.extend_from_slice(&(data.len() as u32).to_le_bytes());

    let (out, body) = (data.len(), body.len());
    // The most the inflater can have taken by the end: its body and the trailer it reads ahead.
    let most_by_ratio = ((body + 8) as u64).saturating_mul(limits.most_ratio).saturating_add(RATIO_SLACK);
    let header_past = name.is_some_and(|len| len as u64 + 1 > u64::from(limits.most_header));
    let early = |why| Broken::Oracle(BoundBroken::RefusedEarly { why, limits, out, body, name });
    let mut back = empty();
    match gunzip(&member, limits, heap(), &mut back) {
        Ok(()) => {
            let passed = |bound| Err(Broken::Oracle(BoundBroken::Passed { bound, limits, out, body, name }));
            if header_past {
                return passed("most header");
            }
            if out as u64 > limits.most_out {
                return passed("most out");
            }
            if out as u64 > most_by_ratio {
                return passed("most ratio");
            }
            if back[..] != data[..] {
                return Err(Broken::Oracle(BoundBroken::Differs { len: out, back: back.len() }));
            }
            Ok(())
        }
        Err(Error { why: why @ Why::GzipHeaderOver { most }, .. }) => {
            tape.reach(BoundPoint::GzipHeaderOver);
            match header_past && most == limits.most_header {
                true => Ok(()),
                false => Err(early(why)),
            }
        }
        Err(Error { why: why @ Why::OutputOver { most }, .. }) => {
            tape.reach(BoundPoint::OutputOver);
            match out as u64 > limits.most_out && most == limits.most_out && back.len() as u64 <= most {
                true => Ok(()),
                false => Err(early(why)),
            }
        }
        Err(Error { why: why @ Why::RatioOver { input, output, most_ratio }, .. }) => {
            tape.reach(BoundPoint::RatioOver);
            let said = input.saturating_mul(most_ratio).saturating_add(RATIO_SLACK);
            match most_ratio == limits.most_ratio && output > said && back.len() as u64 <= said {
                true => Ok(()),
                false => Err(early(why)),
            }
        }
        Err(e) => Err(Broken::Oracle(BoundBroken::Refused { why: e.why })),
    }
}

#[test]
fn each_bound_refuses_at_itself_and_nowhere_else() {
    check(Config::cases(256).must_reach_points(<BoundPoint as Point>::IDS), bounds);
}

#[test]
fn zlib_round_trip_and_prefix() {
    check(Config::cases(32), |tape| -> Result<(), Broken<TripBroken>> {
        let data = shaped(tape);
        let level = level(tape);
        let mut first = empty();
        zlib(&data, level, heap(), &mut first).map_err(|e| Broken::Harness(format!("{e}")))?;
        let theirs = miniz_oxide::inflate::decompress_to_vec_zlib(&first)
            .map_err(|e| Broken::Oracle(TripBroken::ReferenceRefuses { error: format!("{e:?}") }))?;
        if theirs != data {
            return Err(Broken::Oracle(TripBroken::ReferenceDiffers));
        }
        let refused = |e: Error| Broken::Oracle(TripBroken::Refused { why: e.why });
        // Two streams end to end, as a git pack has them: the first's length is found.
        let mut both = Vec::from_slice_in(&first, heap());
        zlib(b"second", Level::Fast, heap(), &mut both).map_err(|e| Broken::Harness(format!("{e}")))?;
        let mut out = empty();
        let len = unzlib_prefix(&both, LOOSE, heap(), &mut out).map_err(refused)?;
        if len != first.len() || out[..] != data[..] {
            return Err(Broken::Oracle(TripBroken::Differs { len: first.len(), back: len }));
        }
        let mut second = empty();
        unzlib(&both[len..], LOOSE, heap(), &mut second).map_err(refused)?;
        if &second[..] != b"second" {
            return Err(Broken::Oracle(TripBroken::Differs { len: 6, back: second.len() }));
        }
        // Fed a byte at a time, the same.
        let mut z = ZlibDecoder::new_in(LOOSE, heap());
        let mut piecewise = empty();
        for byte in first.iter() {
            z.feed(core::slice::from_ref(byte), &mut piecewise).map_err(refused)?;
        }
        z.finish().map_err(refused)?;
        if piecewise[..] != data[..] {
            return Err(Broken::Oracle(TripBroken::Differs { len: data.len(), back: piecewise.len() }));
        }
        // Into a slice, a byte at a time, the same; the bytes after it stay its next thing's.
        let mut z = ZlibInto::new(LOOSE);
        let mut flat = std::vec![0u8; data.len()];
        let mut at = 0;
        while !z.finish().is_ok() && at < both.len() {
            at += z.feed(&both[at..at + 1], &mut flat).map_err(refused)?.consumed;
        }
        z.finish().map_err(refused)?;
        if flat != data || z.written() as usize != data.len() || z.stream_len() as usize != first.len() {
            return Err(Broken::Oracle(TripBroken::SliceDiffers { len: data.len(), back: z.written() as usize }));
        }
        Ok(())
    });
}

#[test]
fn zlib_checks_its_adler32() {
    let mut stream = empty();
    zlib(b"some bytes to check", Level::Default, heap(), &mut stream).expect("zlib");
    let last = stream.len() - 1;
    stream[last] ^= 1;
    let got = unzlib(&stream, LOOSE, heap(), &mut empty()).map_err(|e| e.why);
    assert!(matches!(got, Err(Why::Adler32 { .. })), "{got:?}");
    assert_eq!(
        unzlib(&[0x78, 0x9d, 0x03, 0x00], LOOSE, heap(), &mut empty()).map_err(|e| e.why),
        Err(Why::ZlibHeader { cmf: 0x78, flg: 0x9d })
    );
}

/// The gzip reader's invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum GzipBroken {
    /// A member read as other bytes.
    Differs { len: usize, back: usize },
    /// A good member refused.
    Refused { why: Why },
    /// A member whose CRC was corrupted read as good.
    CorruptAccepted,
    /// A member whose CRC was corrupted refused, but not for its CRC.
    CorruptRefusedOtherwise { why: Why },
}

impl fmt::Display for GzipBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GzipBroken::Differs { len, back } => write!(f, "gzip round trip differs: {len} bytes in, {back} back"),
            GzipBroken::Refused { why } => write!(f, "gzip round trip refused: {why}"),
            GzipBroken::CorruptAccepted => write!(f, "corrupt CRC not refused: the member was read"),
            GzipBroken::CorruptRefusedOtherwise { why } => write!(f, "corrupt CRC refused for another reason: {why}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the gzip reader.
    enum GzipPlant breaks GzipBroken {
        /// The reader as it is.
        None,
        /// Any trailer CRC is taken: a corrupted one is read as good.
        GzipCrcUnchecked => GzipBroken::CorruptAccepted,
    }
}

impl GzipPlant {
    /// The codec's plant this control plants.
    fn planted(self) -> DeflatePlant {
        match self {
            GzipPlant::None => DeflatePlant::None,
            GzipPlant::GzipCrcUnchecked => DeflatePlant::GzipCrcUnchecked,
        }
    }
}

/// gzip: a member written here or by hand, read back in pieces, its CRC checked.
fn gzip_case(tape: &Tape, plant: impl Planted<DeflatePlant>) -> Result<(), Broken<GzipBroken>> {
    let data = shaped(tape);
    let mut member = empty();
    gzip(&data, level(tape), heap(), &mut member).map_err(|e| Broken::Harness(format!("{e}")))?;
    let corrupt = tape.coin("corrupt the CRC");
    if corrupt {
        let at = member.len() - 8 + tape.index(4, "CRC byte");
        member[at] ^= 1 << tape.below(8, "bit");
    }
    let mut g = PlantedGzipDecoder::new_planted(LOOSE, heap(), plant);
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
        (false, Ok(())) => Err(Broken::Oracle(GzipBroken::Differs { len: data.len(), back: out.len() })),
        (false, Err(e)) => Err(Broken::Oracle(GzipBroken::Refused { why: e.why })),
        (true, Err(Error { why: Why::Crc32 { .. }, .. })) => Ok(()),
        (true, Ok(())) => Err(Broken::Oracle(GzipBroken::CorruptAccepted)),
        (true, Err(e)) => Err(Broken::Oracle(GzipBroken::CorruptRefusedOtherwise { why: e.why })),
    }
}

#[test]
fn gzip_round_trip_and_crc() {
    check(Config::cases(48), |tape| gzip_case(tape, NoPlant));
}

#[test]
fn every_gzip_plant_is_caught_by_its_invariant() {
    controls(Config::cases(48), |tape, plant: GzipPlant| gzip_case(tape, plant.planted()));
}

#[test]
fn gzip_header_fields_and_bounds() {
    // A member with every optional field, as other writers make it.
    let mut body = empty();
    crate::deflate(b"payload", Level::Fast, heap(), &mut body).expect("deflate");
    let mut member = StdVec::from([0x1f, 0x8b, 8, 0x02 | 0x04 | 0x08 | 0x10, 1, 2, 3, 4, 0, 3]);
    member.extend_from_slice(&[3, 0, b'x', b'y', b'z']);
    member.extend_from_slice(b"name.tar\0a comment\0");
    let hcrc = (crc32(&member) & 0xffff) as u16;
    member.extend_from_slice(&hcrc.to_le_bytes());
    member.extend_from_slice(&body);
    member.extend_from_slice(&crc32(b"payload").to_le_bytes());
    member.extend_from_slice(&7u32.to_le_bytes());
    let mut out = empty();
    gunzip(&member, LOOSE, heap(), &mut out).expect("a member with every field");
    assert_eq!(&out[..], b"payload");

    // The header bound: a name with no end is refused at it, not buffered.
    let mut endless = StdVec::from([0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 3]);
    endless.extend(core::iter::repeat_n(b'n', 10_000));
    let limits = Limits { most_header: 100, ..LOOSE };
    let got = gunzip(&endless, limits, heap(), &mut empty()).map_err(|e| e.why);
    assert_eq!(got, Err(Why::GzipHeaderOver { most: 100 }));

    // A wrong header CRC, a wrong magic, a reserved flag.
    let at = member.len() - body.len() - 9;
    member[at] ^= 0xff;
    assert!(matches!(gunzip(&member, LOOSE, heap(), &mut empty()).map_err(|e| e.why), Err(Why::GzipHeaderCrc { .. })));
    assert_eq!(
        gunzip(&[0x1f, 0x8c], LOOSE, heap(), &mut empty()).map_err(|e| e.why),
        Err(Why::GzipMagic { id1: 0x1f, id2: 0x8c })
    );
    assert_eq!(gunzip(&[0x1f, 0x8b, 8, 0x20], LOOSE, heap(), &mut empty()).map_err(|e| e.why), Err(Why::GzipFlags(0x20)));
}

#[test]
fn deflate_is_deterministic_across_runs() {
    // Equal input and level, equal bytes: the record keys of compressed records rest on it.
    let data: StdVec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(i) >> 7) as u8).collect();
    for level in [Level::Store, Level::Fast, Level::Default] {
        let a = compress(&data, level, &[], NoPlant);
        let b = compress(&data, level, &[1, 2, 3, 65_536, 100_000], NoPlant);
        assert!(a[..] == b[..], "{level:?}: bytes differ by split");
    }
}

/// What one decoder made of a stream fed in pieces: each feed's answer, then the output.
fn run_into<P: Planted<DeflatePlant>>(
    mut i: PlantedInflateInto<P>,
    stream: &[u8],
    room: usize,
    splits: &[usize],
) -> (StdVec<Result<Fed, Error>>, StdVec<u8>) {
    let mut out = std::vec![0u8; room];
    let mut feeds = StdVec::new();
    let mut at = 0usize;
    let mut sizes = splits.iter().copied().chain(core::iter::repeat(usize::MAX));
    while at < stream.len() && !i.is_done() {
        let end = at.saturating_add(sizes.next().unwrap_or(usize::MAX).max(1)).min(stream.len());
        let fed = i.feed(&stream[at..end], &mut out);
        feeds.push(fed);
        match fed {
            Ok(fed) => at += fed.consumed,
            Err(_) => break,
        }
    }
    feeds.push(i.finish().map(|()| Fed { consumed: 0, done: true }));
    out.truncate(i.written() as usize);
    (feeds, out)
}

/// The fast loop's invariants against its twin.
#[derive(Clone, PartialEq, Eq, Debug)]
enum FastBroken {
    /// The fast loop answered a feed otherwise than the careful decoder.
    Answers { fast: String, careful: String },
    /// The fast loop wrote other bytes.
    Bytes { fast: usize, careful: usize },
}

impl fmt::Display for FastBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FastBroken::Answers { fast, careful } => {
                write!(f, "the fast loop answers {fast}, the careful decoder {careful}")
            }
            FastBroken::Bytes { fast, careful } => {
                write!(f, "the fast loop wrote other bytes ({fast} and {careful})")
            }
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the fast loop.
    enum FastPlant breaks FastBroken {
        /// The fast loop as it is.
        None,
        /// Every match copied from one byte farther back: the answers agree, the bytes do not.
        FastDistanceOffByOne => FastBroken::Bytes { .. },
    }
}

impl FastPlant {
    /// The codec's plant this control plants.
    fn planted(self) -> DeflatePlant {
        match self {
            FastPlant::None => DeflatePlant::None,
            FastPlant::FastDistanceOffByOne => DeflatePlant::FastDistanceOffByOne,
        }
    }
}

/// The fast loop's oracle: the careful decoder alone, fed the same, answers the same at every
/// feed (equal errors at equal offsets) and writes the same bytes.
fn fast_equals_careful(
    stream: &[u8],
    limits: Limits,
    room: usize,
    splits: &[usize],
    plant: impl Planted<DeflatePlant>,
) -> Result<(), Broken<FastBroken>> {
    let fast = run_into(PlantedInflateInto::new_planted(limits, plant), stream, room, splits);
    let careful = run_into(crate::InflateInto::careful(limits), stream, room, splits);
    if fast.0 != careful.0 {
        return Err(Broken::Oracle(FastBroken::Answers {
            fast: format!("{:?}", fast.0),
            careful: format!("{:?}", careful.0),
        }));
    }
    if fast.1 != careful.1 {
        return Err(Broken::Oracle(FastBroken::Bytes { fast: fast.1.len(), careful: careful.1.len() }));
    }
    Ok(())
}

/// The streams the twin test reads: the crate's own source by every encoder at hand.
fn corpus() -> StdVec<StdVec<u8>> {
    let files: [&[u8]; 4] =
        [include_bytes!("inflate.rs"), include_bytes!("deflate.rs"), include_bytes!("tables.rs"), include_bytes!("tests.rs")];
    let mut streams = StdVec::new();
    for file in files {
        for level in [Level::Store, Level::Fast, Level::Default] {
            streams.push(compress(file, level, &[], NoPlant).as_slice().to_vec());
        }
        for level in [1, 6, 9] {
            streams.push(miniz_oxide::deflate::compress_to_vec(file, level));
        }
    }
    streams
}

#[test]
fn the_fast_loop_equals_the_careful_decoder() {
    let streams = corpus();
    check(Config::cases(300), |tape| {
        let mut stream = match tape.coin("corpus or shaped") {
            true => streams[tape.index(streams.len(), "stream")].clone(),
            false => compress(&shaped(tape), level(tape), &[], NoPlant).as_slice().to_vec(),
        };
        // A truncation, a corrupted byte, both or neither.
        if tape.coin("truncate") && !stream.is_empty() {
            stream.truncate(tape.index(stream.len(), "keep"));
        }
        if tape.coin("corrupt") && !stream.is_empty() {
            let at = tape.index(stream.len(), "corrupt at");
            stream[at] ^= 1 + tape.int_in(0u8..=254, "by");
        }
        // Room short of, at, or past the output, and bounds the stream may meet.
        let room = tape.int_in(0usize..=400_000, "room");
        let limits = Limits {
            most_out: tape.int_in(0u64..=400_000, "most out"),
            most_ratio: *tape.pick(&[1u64, 4, 64, MAX_RATIO], "ratio").expect("ratios"),
            most_header: 0,
        };
        fast_equals_careful(&stream, limits, room, &splits(tape, "feed splits"), NoPlant)
    });
}

/// The fast loop's control: a periodic input at the fast level, all matches of short distance.
fn fast_periodic(tape: &Tape, plant: impl Planted<DeflatePlant>) -> Result<(), Broken<FastBroken>> {
    let period = tape.bytes(2..=15, "period");
    let data: StdVec<u8> = (0..tape.int_in(2_000usize..=20_000, "len")).map(|i| period[i % period.len()]).collect();
    let stream = compress(&data, Level::Fast, &[], NoPlant);
    fast_equals_careful(&stream, LOOSE, data.len() + 300, &[], plant)
}

#[test]
fn every_fast_loop_plant_is_caught_by_its_invariant() {
    controls(Config::cases(64), |tape, plant: FastPlant| fast_periodic(tape, plant.planted()));
}

#[test]
fn the_fast_loop_equals_the_careful_decoder_at_every_truncation() {
    let file = include_bytes!("tables.rs");
    for stream in
        [compress(file, Level::Default, &[], NoPlant).as_slice().to_vec(), miniz_oxide::deflate::compress_to_vec(file, 9)]
    {
        for keep in 0..=stream.len() {
            if let Err(Broken::Oracle(why)) = fast_equals_careful(&stream[..keep], LOOSE, file.len() + 300, &[], NoPlant) {
                panic!("cut at {keep} of {}: {why}", stream.len());
            }
        }
    }
}

/// The vector routines' invariant: each equals its scalar twin.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SimdBroken {
    MatchLen { vector: usize, scalar: usize, a: usize, b: usize, from: usize, most: usize },
    AdlerRun { vector: (u32, u32), scalar: (u32, u32), len: usize },
}

impl fmt::Display for SimdBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimdBroken::MatchLen { vector, scalar, a, b, from, most } => {
                write!(f, "match_len {vector}, its twin {scalar} (a {a}, b {b}, from {from}, most {most})")
            }
            SimdBroken::AdlerRun { vector, scalar, len } => {
                write!(f, "adler_run {vector:?}, its twin {scalar:?} over {len} bytes")
            }
        }
    }
}

/// Each vector routine equals its scalar twin on random input at every alignment.
#[test]
fn vector_routines_equal_their_scalar_twins() {
    use crate::simd::{adler_run, adler_run_scalar, match_len, match_len_scalar};
    check(Config::cases(400), |tape| -> Result<(), Broken<SimdBroken>> {
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
        let (vector, scalar) = (match_len(&data, a, b, from, most), match_len_scalar(&data, a, b, from, most));
        if vector != scalar {
            return Err(Broken::Oracle(SimdBroken::MatchLen { vector, scalar, a, b, from, most }));
        }
        let run = &data[skew..data.len().min(skew + 5_552)];
        let start = (tape.int_in(1u32..=65_520, "a"), tape.int_in(0u32..=65_520, "b"));
        let (vector, scalar) = (adler_run(start.0, start.1, run), adler_run_scalar(start.0, start.1, run));
        if vector != scalar {
            return Err(Broken::Oracle(SimdBroken::AdlerRun { vector, scalar, len: run.len() }));
        }
        Ok(())
    });
}
