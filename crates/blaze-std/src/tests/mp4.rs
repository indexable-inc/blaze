//! `bstd::video::mp4`: the muxer's bytes, read back by a box parser written here (not the
//! muxer's writer, so a mistake in one is not repeated in the other).
//!
//! - Every box nests exactly (each child's size sums to its parent's), in both layouts, and the
//!   samples read back from the tables (progressive: `stts`, `ctts`, `stss`, `stsz`, `stco`;
//!   fragmented: `tfdt` and `trun` of each `moof`) are the packets pushed: their bytes, decode
//!   times from 0, composition offsets, durations and key frames, for random packet runs with
//!   reordering, varying durations and several key frames.
//! - The same packets give the same bytes twice (no clock in them), whether a fragmented file
//!   is taken in pieces as it goes or at the end.
//! - The track: `hvc1` holding the `hvcC` given, `colr`, and HDR10's `mdcv` and `clli` with the
//!   bytes ST 2086 and CTA-861.3 lay out; creation times 0.
//! - Refusals name the sample: a first packet that is not a key frame, decode times that do not
//!   rise, another timescale, a presentation before the first; and an empty file, a bad `hvcC`.

use std::alloc::Global;

use blaze_sim::{Config, Tape};

use crate::video::{Colour, ContentLight, Layout, Mastering, MuxError, Muxer, Packet, Track};

const TIMESCALE: u32 = 90_000;

/// An `hvcC` the muxer accepts: version 1 and the 23-byte header, with one array byte after.
const HVCC: [u8; 24] = [1, 1, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 0, 0];

fn track(colour: Colour) -> Track<'static> {
    Track { width: 1920, height: 1080, timescale: TIMESCALE, hvcc: &HVCC, colour }
}

fn packet(data: Vec<u8>, dts: i64, pts: i64, duration: u32, key: bool) -> Packet<Global> {
    Packet { data, pts, dts, duration, timescale: TIMESCALE, key }
}

/// A run of packets as an encoder with B-frames gives them, in decode order: decode times
/// rising by 1 to 3 ticks of `unit`, presentation at decode plus one or two units (so never
/// before the first's), a key frame every `gop`, bytes random.
fn draw_packets(tape: &Tape) -> Vec<Packet<Global>> {
    let count = tape.int_in(1..=40u64, "packets") as usize;
    let gop = tape.int_in(1..=12u64, "gop") as usize;
    let unit = *tape.one_of(&[1i64, 1001, 3000], "unit");
    let mut dts = tape.int_in(0..=1u64 << 40, "first dts") as i64 - (1 << 39);
    let mut packets = Vec::with_capacity(count);
    for i in 0..count {
        let len = tape.int_in(1..=200u64, "len") as usize;
        let data = (0..len).map(|_| tape.int_in(0..=255u64, "byte") as u8).collect();
        let offset = if i == 0 { unit } else { unit * tape.int_in(1..=2u64, "offset units") as i64 };
        let step = unit * tape.int_in(1..=3u64, "step units") as i64;
        packets.push(packet(data, dts, dts + offset, step as u32, i % gop == 0));
        dts += step;
    }
    packets
}

/// A box: its kind, where it starts, and its body (after the size and kind).
#[derive(Clone, Copy)]
struct Atom<'a> {
    kind: [u8; 4],
    at: usize,
    body: &'a [u8],
    body_at: usize,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// The boxes laid end to end in `bytes` (which start at `base` in the file), each exactly
/// inside them.
fn atoms(bytes: &[u8], base: usize) -> Result<Vec<Atom<'_>>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes.len() - i < 8 {
            return Err(format!("a box header at byte {} is cut short", base + i));
        }
        let kind: [u8; 4] = bytes[i + 4..i + 8].try_into().expect("4 bytes");
        let (size, header) = match be32(bytes, i) {
            1 if bytes.len() - i >= 16 => (be64(bytes, i + 8) as usize, 16),
            1 => return Err(format!("box {} at byte {} has a cut-short largesize", kind.escape_ascii(), base + i)),
            size => (size as usize, 8),
        };
        if size < header || size > bytes.len() - i {
            return Err(format!(
                "box {} at byte {} says it is {size} bytes; {} are left",
                kind.escape_ascii(),
                base + i,
                bytes.len() - i
            ));
        }
        out.push(Atom { kind, at: base + i, body: &bytes[i + header..i + size], body_at: base + i + header });
        i += size;
    }
    Ok(out)
}

/// The boxes inside `atom`, after `skip` bytes of its own fields.
fn children<'a>(atom: &Atom<'a>, skip: usize) -> Vec<Atom<'a>> {
    match atoms(&atom.body[skip..], atom.body_at + skip) {
        Ok(list) => list,
        Err(why) => panic!("inside {}: {why}", atom.kind.escape_ascii()),
    }
}

fn find<'a>(list: &[Atom<'a>], kind: &[u8; 4]) -> Option<Atom<'a>> {
    list.iter().copied().find(|a| &a.kind == kind)
}

fn need<'a>(list: &[Atom<'a>], kind: &[u8; 4]) -> Atom<'a> {
    find(list, kind).unwrap_or_else(|| panic!("no {} box", kind.escape_ascii()))
}

/// The box at `path` below `list`, through plain containers.
fn at<'a>(list: &[Atom<'a>], path: &[&[u8; 4]]) -> Atom<'a> {
    let mut here = need(list, path[0]);
    for kind in &path[1..] {
        here = need(&children(&here, 0), kind);
    }
    here
}

fn kinds(list: &[Atom<'_>]) -> Vec<[u8; 4]> {
    list.iter().map(|a| a.kind).collect()
}

/// One sample as read back.
#[derive(PartialEq, Eq, Debug)]
struct Got {
    data: Vec<u8>,
    dt: i64,
    offset: i64,
    duration: u32,
    key: bool,
}

/// The samples the packets should read back as.
fn expected(packets: &[Packet<Global>]) -> Vec<Got> {
    let d0 = packets[0].dts;
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| Got {
            data: p.data.clone(),
            dt: p.dts - d0,
            offset: p.pts - p.dts,
            duration: match packets.get(i + 1) {
                Some(next) => (next.dts - p.dts) as u32,
                None => p.duration,
            },
            key: p.key,
        })
        .collect()
}

/// The edit list's media time: where the presentation starts on the media timeline.
fn media_time(top: &[Atom<'_>]) -> u64 {
    let elst = at(top, &[b"moov", b"trak", b"edts", b"elst"]);
    assert_eq!(elst.body[0], 1, "elst is version 1");
    assert_eq!(be32(elst.body, 4), 1, "one edit");
    be64(elst.body, 16)
}

/// A progressive file's samples, from its tables and its one chunk.
fn read_progressive(file: &[u8]) -> (Vec<Got>, u64) {
    let top = atoms(file, 0).expect("the file is boxes end to end");
    assert_eq!(kinds(&top), [*b"ftyp", *b"moov", *b"mdat"], "a progressive file's top level");
    let stbl = at(&top, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
    let tables = children(&stbl, 0);
    let stsz = need(&tables, b"stsz");
    assert_eq!(be32(stsz.body, 4), 0, "sizes are each sample's");
    let count = be32(stsz.body, 8) as usize;
    let sizes: Vec<usize> = (0..count).map(|i| be32(stsz.body, 12 + 4 * i) as usize).collect();
    let stts = need(&tables, b"stts");
    let mut durations = Vec::new();
    for e in 0..be32(stts.body, 4) as usize {
        let (n, delta) = (be32(stts.body, 8 + 8 * e), be32(stts.body, 12 + 8 * e));
        durations.extend(std::iter::repeat_n(delta, n as usize));
    }
    let mut offsets = vec![0i64; count];
    if let Some(ctts) = find(&tables, b"ctts") {
        let mut i = 0;
        for e in 0..be32(ctts.body, 4) as usize {
            let (n, offset) = (be32(ctts.body, 8 + 8 * e), be32(ctts.body, 12 + 8 * e));
            for _ in 0..n {
                offsets[i] = i64::from(offset);
                i += 1;
            }
        }
        assert_eq!(i, count, "ctts covers every sample");
    }
    let mut keys = vec![true; count];
    if let Some(stss) = find(&tables, b"stss") {
        keys = vec![false; count];
        for e in 0..be32(stss.body, 4) as usize {
            keys[be32(stss.body, 8 + 4 * e) as usize - 1] = true;
        }
    }
    let stsc = need(&tables, b"stsc");
    assert_eq!(be32(stsc.body, 4), 1, "one stsc run");
    assert_eq!(be32(stsc.body, 12) as usize, count, "every sample in the one chunk");
    let stco = need(&tables, b"stco");
    assert_eq!(be32(stco.body, 4), 1, "one chunk");
    let chunk = be32(stco.body, 8) as usize;
    let mdat = need(&top, b"mdat");
    assert_eq!(chunk, mdat.body_at, "stco points at mdat's first byte");
    assert_eq!(durations.len(), count, "stts covers every sample");
    let mut got = Vec::with_capacity(count);
    let (mut at_byte, mut dt) = (chunk, 0i64);
    for i in 0..count {
        got.push(Got {
            data: file[at_byte..at_byte + sizes[i]].to_vec(),
            dt,
            offset: offsets[i],
            duration: durations[i],
            key: keys[i],
        });
        at_byte += sizes[i];
        dt += i64::from(durations[i]);
    }
    assert_eq!(at_byte, mdat.at + 8 + mdat.body.len(), "the samples fill mdat");
    (got, media_time(&top))
}

/// A fragmented file's samples, from each `moof` and the `mdat` after it.
fn read_fragmented(file: &[u8]) -> (Vec<Got>, u64) {
    let top = atoms(file, 0).expect("the file is boxes end to end");
    assert_eq!(&kinds(&top)[..2], [*b"ftyp", *b"moov"], "a fragmented file starts with its init segment");
    let moov = children(&need(&top, b"moov"), 0);
    let trex = at(&moov, &[b"mvex", b"trex"]);
    assert_eq!(be32(trex.body, 4), 1, "trex is the one track's");
    let stsz = at(&moov, &[b"trak", b"mdia", b"minf", b"stbl", b"stsz"]);
    assert_eq!(be32(stsz.body, 8), 0, "the init segment's tables are empty");
    let mut got = Vec::new();
    for (n, pair) in top[2..].chunks(2).enumerate() {
        assert_eq!(kinds(pair), [*b"moof", *b"mdat"], "fragment {n} is a moof and its mdat");
        let (moof, mdat) = (pair[0], pair[1]);
        let inside = children(&moof, 0);
        let mfhd = need(&inside, b"mfhd");
        assert_eq!(be32(mfhd.body, 4) as usize, n + 1, "fragments are numbered from 1");
        let traf = children(&need(&inside, b"traf"), 0);
        let tfhd = need(&traf, b"tfhd");
        assert_eq!(be32(tfhd.body, 0) & 0x00FF_FFFF, 0x02_0000, "data offsets count from the moof");
        let tfdt = need(&traf, b"tfdt");
        assert_eq!(tfdt.body[0], 1, "tfdt is version 1");
        let mut dt = be64(tfdt.body, 4) as i64;
        let trun = need(&traf, b"trun");
        assert_eq!(be32(trun.body, 0) & 0x00FF_FFFF, 0xF01, "trun has data offset and every sample field");
        let count = be32(trun.body, 4) as usize;
        let mut at_byte = moof.at + be32(trun.body, 8) as usize;
        assert_eq!(at_byte, mdat.body_at, "fragment {n}'s data offset points at its mdat");
        for i in 0..count {
            let e = 12 + 16 * i;
            let (duration, size, flags, offset) =
                (be32(trun.body, e), be32(trun.body, e + 4) as usize, be32(trun.body, e + 8), be32(trun.body, e + 12));
            got.push(Got {
                data: file[at_byte..at_byte + size].to_vec(),
                dt,
                offset: i64::from(offset),
                duration,
                key: flags & 0x0001_0000 == 0,
            });
            at_byte += size;
            dt += i64::from(duration);
        }
        assert_eq!(at_byte, mdat.at + 8 + mdat.body.len(), "fragment {n}'s samples fill its mdat");
    }
    (got, media_time(&top))
}

/// The whole file, taking the fragmented muxer's bytes as they come when `pieces`.
fn mux(packets: &[Packet<Global>], layout: Layout, colour: Colour, pieces: bool) -> Result<Vec<u8>, MuxError> {
    let mut muxer = Muxer::new(&track(colour), layout, Global)?;
    let mut file = Vec::new();
    for p in packets {
        muxer.push(p)?;
        if pieces {
            file.extend_from_slice(&muxer.bytes());
        }
    }
    file.extend_from_slice(&muxer.finish()?);
    Ok(file)
}

#[test]
fn muxed_boxes_read_back_as_the_packets() {
    blaze_sim::check(Config::cases(300), |tape| {
        let packets = draw_packets(tape);
        let layout = *tape.one_of(&[Layout::Progressive, Layout::Fragmented], "layout");
        let colour = if tape.coin("hdr10") {
            Colour::hdr10(Mastering::P3_D65_1000, Some(ContentLight { max_cll: 1000, max_fall: 400 }))
        } else {
            Colour::BT709
        };
        let pieces = tape.coin("in pieces");
        let file = mux(&packets, layout, colour, pieces).expect("valid packets mux");
        let (got, start) = match layout {
            Layout::Progressive => read_progressive(&file),
            Layout::Fragmented => read_fragmented(&file),
        };
        assert_eq!(got, expected(&packets), "the samples read back are the packets ({layout:?})");
        assert_eq!(start as i64, packets[0].pts - packets[0].dts, "the edit starts at the first presentation");
        let again = mux(&packets, layout, colour, !pieces).expect("valid packets mux");
        assert!(file == again, "the same packets give the same bytes, in pieces or whole ({layout:?})");
    });
}

#[test]
fn the_track_holds_its_hvcc_and_hdr10_colour() {
    let light = ContentLight { max_cll: 1000, max_fall: 400 };
    let colour = Colour::hdr10(Mastering::P3_D65_1000, Some(light));
    let packets = [packet(vec![0, 0, 0, 1, 0x26], 0, 0, 3000, true)];
    let file = mux(&packets, Layout::Progressive, colour, false).expect("a packet muxes");
    let top = atoms(&file, 0).expect("boxes");
    let ftyp = need(&top, b"ftyp");
    assert_eq!(&ftyp.body[..4], b"isom");
    let mvhd = at(&top, &[b"moov", b"mvhd"]);
    assert_eq!(be32(mvhd.body, 4), 0, "mvhd's creation time is 0, not the clock");
    assert_eq!(be32(mvhd.body, 8), 0, "mvhd's modification time is 0");
    assert_eq!(be32(mvhd.body, 12), TIMESCALE);
    assert_eq!(be32(mvhd.body, 16), 3000, "the movie lasts its one sample");
    let tkhd = at(&top, &[b"moov", b"trak", b"tkhd"]);
    assert_eq!((be32(tkhd.body, 4), be32(tkhd.body, 8)), (0, 0), "tkhd's times are 0");
    assert_eq!((be32(tkhd.body, 76), be32(tkhd.body, 80)), (1920 << 16, 1080 << 16), "tkhd's size, 16.16");
    let hdlr = at(&top, &[b"moov", b"trak", b"mdia", b"hdlr"]);
    assert_eq!(&hdlr.body[8..12], b"vide");
    let stsd = at(&top, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"]);
    assert_eq!(be32(stsd.body, 4), 1, "one sample entry");
    let entries = children(&stsd, 8);
    assert_eq!(kinds(&entries), [*b"hvc1"]);
    let hvc1 = entries[0];
    assert_eq!(u16::from_be_bytes([hvc1.body[24], hvc1.body[25]]), 1920);
    assert_eq!(u16::from_be_bytes([hvc1.body[26], hvc1.body[27]]), 1080);
    let inside = children(&hvc1, 78);
    assert_eq!(kinds(&inside), [*b"hvcC", *b"colr", *b"mdcv", *b"clli"]);
    assert_eq!(inside[0].body, HVCC, "hvcC is the record given");
    assert_eq!(inside[1].body, [b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0], "colr: BT.2020, PQ, BT.2020, limited");
    assert_eq!(inside[2].body, Mastering::P3_D65_1000.to_bytes());
    assert_eq!(
        inside[2].body,
        [0x33, 0xc2, 0x86, 0xc4, 0x1d, 0x4c, 0x0b, 0xb8, 0x84, 0xd0, 0x3e, 0x80, 0x3d, 0x13, 0x40, 0x42, 0x00, 0x98, 0x96, 0x80, 0, 0, 0, 1],
        "mdcv: G, B, R, white (0.00002), max and min luminance (0.0001 nits), as ST 2086 lays out"
    );
    assert_eq!(inside[3].body, [0x03, 0xe8, 0x01, 0x90], "clli: 1000 and 400 nits");
    let sdr = mux(&packets, Layout::Progressive, Colour::BT709, false).expect("a packet muxes");
    let top = atoms(&sdr, 0).expect("boxes");
    let stsd = at(&top, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"]);
    let inside = children(&children(&stsd, 8)[0], 78);
    assert_eq!(kinds(&inside), [*b"hvcC", *b"colr"], "SDR has no HDR10 boxes");
}

#[test]
fn refusals_name_the_sample() {
    let key = |dts, pts| packet(vec![1], dts, pts, 10, true);
    let other = |dts, pts| packet(vec![1], dts, pts, 10, false);
    let run = |packets: &[Packet<Global>]| mux(packets, Layout::Progressive, Colour::BT709, false);
    assert!(matches!(run(&[other(0, 0)]), Err(MuxError::Order { sample: 0, .. })), "the first is not a key frame");
    assert!(
        matches!(run(&[key(0, 0), other(10, 10), other(10, 20)]), Err(MuxError::Order { sample: 2, .. })),
        "a decode time that does not rise"
    );
    assert!(
        matches!(run(&[key(0, 10), other(10, 5)]), Err(MuxError::Order { sample: 1, .. })),
        "a presentation before the first's"
    );
    assert!(
        matches!(run(&[key(0, 0), other(10, 5)]), Err(MuxError::Order { sample: 1, .. })),
        "a presentation before its decode"
    );
    let mut foreign = key(10, 10);
    foreign.timescale = 600;
    assert_eq!(
        run(&[key(0, 0), foreign]),
        Err(MuxError::Timescale { sample: 1, got: 600, track: TIMESCALE }),
        "another timescale"
    );
    assert_eq!(run(&[]), Err(MuxError::Empty), "an mp4 needs a packet");
    let short = Track { hvcc: &HVCC[..10], ..track(Colour::BT709) };
    assert!(matches!(Muxer::new(&short, Layout::Fragmented, Global), Err(MuxError::Config { field: "hvcc", .. })));
    let wide = Track { width: 70_000, ..track(Colour::BT709) };
    assert!(matches!(Muxer::new(&wide, Layout::Progressive, Global), Err(MuxError::Config { field: "width", .. })));
}
