//! The crate's promises, each with its oracle and a planted bug it catches:
//!
//! - read of write is the entries written, for any split of the input, and agrees with the
//!   `tar` crate (a dev-dependency) in both directions;
//! - the writer's bytes are a function of the entries;
//! - every escape is refused, naming its entry;
//! - each bound refuses an archive past it, and only such an archive: the bounds are drawn down
//!   to their least legal values (D165) and each refusal is reached.

use core::fmt;
use std::{borrow::ToOwned, format, string::String, vec::Vec as StdVec};

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls, points};

use crate::{
    Error, Event, Field, Kind, Limits, Links, Make, PathWhy, PlantedReader, PlantedWriter, TarPlant, Why,
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const LIMITS: Limits = Limits {
    most_path: 4096,
    most_meta: 1 << 16,
    most_entry: 1 << 24,
    most_total: 1 << 26,
    most_entries: 10_000,
    links: Links::Inside,
};

/// An entry as read back: path, what it is, its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Got {
    path: String,
    what: String,
    bytes: StdVec<u8>,
}

/// Every entry of `archive`, fed in pieces of the sizes `splits` gives.
fn read_all(
    archive: &[u8],
    limits: Limits,
    splits: &[usize],
    plant: impl Planted<TarPlant>,
) -> Result<StdVec<Got>, Error> {
    let mut reader = PlantedReader::new_planted(limits, heap(), plant);
    let mut got: StdVec<Got> = StdVec::new();
    let mut at = 0usize;
    let mut sizes = splits.iter().copied().chain(core::iter::repeat(usize::MAX));
    loop {
        let end = at.saturating_add(sizes.next().unwrap_or(usize::MAX).max(1)).min(archive.len());
        let mut piece = &archive[at..end];
        let mut ended = false;
        loop {
            let step = reader.step(piece)?;
            piece = &piece[step.consumed..];
            at += step.consumed;
            match step.event {
                None => break,
                Some(Event::End) => {
                    ended = true;
                    break;
                }
                Some(Event::Entry(e)) => got.push(Got {
                    path: e.path.to_owned(),
                    what: match e.kind {
                        Kind::File => format!("file {:o}", e.mode),
                        Kind::Dir => String::from("dir"),
                        Kind::Symlink { resolved, .. } => format!("symlink {resolved}"),
                        Kind::HardLink { target } => format!("hardlink {target}"),
                    },
                    bytes: StdVec::new(),
                }),
                Some(Event::Data { bytes, .. }) => {
                    got.last_mut().expect("data after an entry").bytes.extend_from_slice(bytes);
                }
            }
        }
        if ended || end == archive.len() {
            break;
        }
    }
    reader.finish()?;
    Ok(got)
}

type Entries = StdVec<(String, Make<'static>, StdVec<u8>)>;

/// How many entries, how deep and how large the generator makes them.
#[derive(Clone, Copy)]
struct Shape {
    most_entries: usize,
    most_depth: usize,
    most_bytes: usize,
}

/// The round trip's shape: paths that need ustar's prefix and pax, files of several blocks.
const ROUND_TRIP: Shape = Shape { most_entries: 12, most_depth: 4, most_bytes: 3000 };

/// Entries to write: files at depths and lengths that need ustar's prefix and pax, and
/// directories.
fn entries(tape: &Tape, shape: Shape) -> Entries {
    let mut out = StdVec::new();
    let count = tape.int_in(0usize..=shape.most_entries, "entries");
    for i in 0..count {
        let depth = tape.int_in(0usize..=shape.most_depth, "depth");
        let mut path = String::new();
        for d in 0..depth {
            let len = *tape.pick(&[1usize, 8, 40, 99, 160], "dir len").expect("lens");
            path.push_str(&"d".repeat(len));
            path.push_str(&format!("{d}/"));
        }
        let len = *tape.pick(&[1usize, 30, 99, 100, 101, 200], "name len").expect("lens");
        path.push_str(&"f".repeat(len));
        path.push_str(&format!("{i}"));
        match tape.below(4, "make") {
            0 => out.push((path, Make::Dir, StdVec::new())),
            1 => out.push((path, Make::File { exec: true }, tape.bytes(0..=shape.most_bytes, "bytes"))),
            _ => out.push((path, Make::File { exec: false }, tape.bytes(0..=shape.most_bytes, "bytes"))),
        }
    }
    out
}

fn write_all(entries: &[(String, Make<'_>, StdVec<u8>)], plant: impl Planted<TarPlant>) -> Vec<u8, Alloc<'static>> {
    let mut w = PlantedWriter::new_planted(plant);
    let mut out = Vec::new_in(heap());
    for (path, make, bytes) in entries {
        w.entry(path, *make, bytes, &mut out).expect("written");
    }
    w.finish(&mut out).expect("finished");
    out
}

fn expected(entries: &[(String, Make<'_>, StdVec<u8>)]) -> StdVec<Got> {
    entries
        .iter()
        .map(|(path, make, bytes)| Got {
            path: path.clone(),
            what: match make {
                Make::File { exec: true } => String::from("file 755"),
                Make::File { exec: false } => String::from("file 644"),
                Make::Dir => String::from("dir"),
                Make::Symlink { target } => format!("symlink {target}"),
            },
            bytes: bytes.clone(),
        })
        .collect()
}

/// The round trip's invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TripBroken {
    /// The writer wrote other bytes for the same entries.
    NotDeterministic,
    /// Our reader refused our archive.
    Refused { why: Why },
    /// Our reader read other entries than were written.
    Differs { read: usize, written: usize },
    /// The reference failed on our archive.
    Reference { error: String },
    /// The reference read entry `entry` otherwise.
    ReferenceDiffers { entry: usize, path: String },
    /// The reference read another count of entries.
    ReferenceCount { read: usize, written: usize },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::NotDeterministic => write!(f, "the writer's bytes differ between runs"),
            TripBroken::Refused { why } => write!(f, "round trip refused: {why}"),
            TripBroken::Differs { read, written } => {
                write!(f, "round trip differs: {read} entries read, {written} written")
            }
            TripBroken::Reference { error } => write!(f, "the reference: {error}"),
            TripBroken::ReferenceDiffers { entry, path } => write!(f, "the reference reads entry {entry} as `{path}`"),
            TripBroken::ReferenceCount { read, written } => {
                write!(f, "the reference reads {read} entries, not {written}")
            }
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the round trip.
    enum TripPlant breaks TripBroken {
        /// The writer as it is.
        None,
        /// The size field one short: a file reads back a byte short.
        SizeShort => TripBroken::Differs { .. },
    }
}

impl TripPlant {
    /// The crate's plant this control plants.
    fn planted(self) -> TarPlant {
        match self {
            TripPlant::None => TarPlant::None,
            TripPlant::SizeShort => TarPlant::SizeShort,
        }
    }
}

fn round_trip(tape: &Tape, plant: impl Planted<TarPlant>) -> Result<(), Broken<TripBroken>> {
    let entries = entries(tape, ROUND_TRIP);
    let archive = write_all(&entries, plant);
    if archive[..] != write_all(&entries, plant)[..] {
        return Err(Broken::Oracle(TripBroken::NotDeterministic));
    }
    let splits = tape.vec(0..=8, "splits", |t| t.int_in(0usize..=2000, "split"));
    let got = read_all(&archive, LIMITS, &splits, NoPlant).map_err(|e| Broken::Oracle(TripBroken::Refused { why: e.why }))?;
    let want = expected(&entries);
    if got != want {
        return Err(Broken::Oracle(TripBroken::Differs { read: got.len(), written: want.len() }));
    }
    // The reference reads ours the same.
    let reference = |e: std::io::Error| Broken::Oracle(TripBroken::Reference { error: format!("{e}") });
    let mut theirs = tar::Archive::new(&archive[..]);
    let mut n = 0;
    for entry in theirs.entries().map_err(reference)? {
        let mut entry = entry.map_err(reference)?;
        let path = entry.path().map_err(reference)?;
        let path = path.to_str().unwrap_or("").trim_end_matches('/').to_owned();
        let mut bytes = StdVec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(reference)?;
        if want.get(n).is_none_or(|w| w.path != path || w.bytes != bytes) {
            return Err(Broken::Oracle(TripBroken::ReferenceDiffers { entry: n, path }));
        }
        n += 1;
    }
    if n != want.len() {
        return Err(Broken::Oracle(TripBroken::ReferenceCount { read: n, written: want.len() }));
    }
    Ok(())
}

#[test]
fn read_of_write_is_the_entries() {
    check(Config::cases(128), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_round_trip_plant_is_caught_by_its_invariant() {
    controls(Config::cases(128), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

#[test]
fn reads_what_the_reference_writes() {
    // GNU long names (`L`), as cargo's `tar` writes a `.crate` with a long path.
    let long = format!("pkg-1.0.0/{}/lib.rs", "a".repeat(150));
    let mut builder = tar::Builder::new(StdVec::new());
    for (path, data) in [("pkg-1.0.0/Cargo.toml", &b"[package]"[..]), (&long[..], &b"fn f() {}"[..])] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, data).expect("the reference writes");
    }
    let archive = builder.into_inner().expect("the reference finishes");
    let got = read_all(&archive, LIMITS, &[], NoPlant).expect("read");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].path, "pkg-1.0.0/Cargo.toml");
    assert_eq!(got[1].path, long);
    assert_eq!(got[1].bytes, b"fn f() {}");
}

/// One raw header block of `kind`, `name` and `link` (old format unless `ustar`), with `data`
/// after it padded: the shapes a hostile archive has, which no writer here makes.
fn raw(kind: u8, name: &[u8], link: &[u8], data: &[u8]) -> StdVec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    h[100..108].copy_from_slice(b"0000644\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link);
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

fn archive(parts: &[StdVec<u8>]) -> StdVec<u8> {
    let mut out: StdVec<u8> = parts.concat();
    out.extend_from_slice(&[0; 1024]);
    out
}

fn refused(parts: &[StdVec<u8>], limits: Limits) -> Why {
    match read_all(&archive(parts), limits, &[], NoPlant) {
        Err(e) => e.why,
        Ok(got) => panic!("not refused: {got:?}"),
    }
}

#[test]
fn every_escape_is_refused() {
    let file = |name: &[u8]| raw(b'0', name, b"", b"x");
    assert_eq!(refused(&[file(b"/etc/passwd")], LIMITS), Why::Path(PathWhy::Absolute));
    assert_eq!(refused(&[file(b"../x")], LIMITS), Why::Path(PathWhy::Parent));
    assert_eq!(refused(&[file(b"a/../../x")], LIMITS), Why::Path(PathWhy::Parent));
    assert_eq!(refused(&[file(b"a\\..\\x")], LIMITS), Why::Path(PathWhy::Backslash));
    assert_eq!(refused(&[file(b"\xff")], LIMITS), Why::Path(PathWhy::NotUtf8));
    let short = Limits { most_path: 10, ..LIMITS };
    assert_eq!(refused(&[file(b"abcdefghijkl")], short), Why::Path(PathWhy::TooLong { len: 12, most: 10 }));
    // A pax path with a NUL in it.
    let pax = raw(b'x', b"pax", b"", b"15 path=a\0/b/c\n");
    assert_eq!(refused(&[pax, file(b"x")], LIMITS), Why::Path(PathWhy::Nul));
    for kind in [b'3', b'4', b'6'] {
        assert_eq!(refused(&[raw(kind, b"dev", b"", b"")], LIMITS), Why::Device(kind));
    }
    let refuse = Limits { links: Links::Refuse, ..LIMITS };
    assert_eq!(refused(&[raw(b'2', b"l", b"x", b"")], refuse), Why::LinksRefused);
    assert_eq!(refused(&[raw(b'1', b"l", b"x", b"")], refuse), Why::LinksRefused);
    assert_eq!(refused(&[raw(b'2', b"a/l", b"../../x", b"")], LIMITS), Why::TargetEscapes);
    assert_eq!(refused(&[raw(b'2', b"l", b"/etc", b"")], LIMITS), Why::Target(PathWhy::Absolute));
    // Through an earlier link, as a file and as a link's target.
    assert_eq!(refused(&[raw(b'2', b"l", b"d", b""), file(b"l/x")], LIMITS), Why::ThroughLink);
    assert_eq!(refused(&[raw(b'2', b"l", b"d", b""), file(b"l")], LIMITS), Why::ThroughLink);
    assert_eq!(refused(&[raw(b'2', b"l", b"d", b""), raw(b'2', b"m", b"l/x", b"")], LIMITS), Why::ThroughLink);
    assert_eq!(refused(&[raw(b'2', b"l", b"d", b""), raw(b'1', b"h", b"l/x", b"")], LIMITS), Why::ThroughLink);
    // A link made where an earlier link's target runs: `m -> x/y/z`, then `x/y -> ../..`.
    assert_eq!(refused(&[raw(b'2', b"m", b"x/y/z", b""), raw(b'2', b"x/y", b"..", b"")], LIMITS), Why::UnderTarget);
    // A link that stays inside is taken, resolved from its directory.
    let got = read_all(&archive(&[raw(b'2', b"a/b/l", b"../c", b"")]), LIMITS, &[], NoPlant).expect("inside");
    assert_eq!(got[0].what, "symlink a/c");
}

/// The escape oracle's invariants: a hostile archive is refused.
#[derive(Clone, PartialEq, Eq, Debug)]
enum EscapeBroken {
    /// An entry whose path has a `..` component was taken.
    ParentTaken { path: String },
    /// An entry whose path runs through an earlier symbolic link was taken.
    ThroughLinkTaken { path: String, link: String },
    /// A hostile archive was read whole with neither of the above.
    Taken { entries: usize },
}

impl fmt::Display for EscapeBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscapeBroken::ParentTaken { path } => write!(f, "escape not refused: `{path}` has a `..`"),
            EscapeBroken::ThroughLinkTaken { path, link } => {
                write!(f, "escape not refused: `{path}` runs through the link `{link}`")
            }
            EscapeBroken::Taken { entries } => write!(f, "escape not refused: {entries} entries read"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the reader's path rules.
    enum EscapePlant breaks EscapeBroken {
        /// The reader as it is.
        None,
        /// `..` taken as a name: `../x` is read.
        ParentUnchecked => EscapeBroken::ParentTaken { .. },
        /// A path through an earlier link taken: `l/x` after `l -> d` is read.
        ThroughLinkUnchecked => EscapeBroken::ThroughLinkTaken { .. },
    }
}

impl EscapePlant {
    /// The crate's plant this control plants.
    fn planted(self) -> TarPlant {
        match self {
            EscapePlant::None => TarPlant::None,
            EscapePlant::ParentUnchecked => TarPlant::ParentUnchecked,
            EscapePlant::ThroughLinkUnchecked => TarPlant::ThroughLinkUnchecked,
        }
    }
}

/// One of two hostile archives, read: refused, or the escape it let through named.
fn escape(tape: &Tape, plant: impl Planted<TarPlant>) -> Result<(), Broken<EscapeBroken>> {
    let all = [
        StdVec::from([raw(b'0', b"../x", b"", b"x")]),
        StdVec::from([raw(b'2', b"l", b"d", b""), raw(b'0', b"l/x", b"", b"x")]),
    ];
    let parts = &all[tape.index(all.len(), "archive")];
    let Ok(got) = read_all(&archive(parts), LIMITS, &[], plant) else {
        return Ok(());
    };
    if let Some(e) = got.iter().find(|e| e.path.split('/').any(|part| part == "..")) {
        return Err(Broken::Oracle(EscapeBroken::ParentTaken { path: e.path.clone() }));
    }
    for (i, e) in got.iter().enumerate() {
        let link = got[..i].iter().find(|l| {
            l.what.starts_with("symlink")
                && e.path.starts_with(&l.path[..])
                && e.path.as_bytes().get(l.path.len()).is_none_or(|&b| b == b'/')
        });
        if let Some(link) = link {
            return Err(Broken::Oracle(EscapeBroken::ThroughLinkTaken { path: e.path.clone(), link: link.path.clone() }));
        }
    }
    Err(Broken::Oracle(EscapeBroken::Taken { entries: got.len() }))
}

#[test]
fn every_escape_plant_is_caught_by_its_invariant() {
    controls(Config::cases(32), |tape, plant: EscapePlant| escape(tape, plant.planted()));
}

#[test]
fn format_faults_are_refused() {
    let mut bad = raw(b'0', b"f", b"", b"x");
    bad[0] = b'g';
    assert!(matches!(refused(&[bad], LIMITS), Why::Checksum { .. }));
    // No end blocks: truncated.
    let only = raw(b'0', b"f", b"", b"x");
    assert_eq!(read_all(&only, LIMITS, &[], NoPlant).map_err(|e| e.why), Err(Why::Truncated));
    // One zero block, then a header.
    let mut half = StdVec::from([0u8; 512]);
    half.extend(raw(b'0', b"f", b"", b"x"));
    assert_eq!(read_all(&half, LIMITS, &[], NoPlant).map_err(|e| e.why), Err(Why::EndBlock));
    let small = Limits { most_entry: 2, most_meta: 8, ..LIMITS };
    assert_eq!(refused(&[raw(b'0', b"f", b"", b"xyz")], small), Why::EntryOver { size: 3, most: 2 });
    assert_eq!(refused(&[raw(b'L', b"././@LongLink", b"", b"0123456789\0")], small), Why::MetaOver { size: 11, most: 8 });
    // The error names its entry: the second, at byte 1024.
    let e = read_all(&archive(&[raw(b'0', b"ok", b"", b"x"), raw(b'0', b"../bad", b"", b"")]), LIMITS, &[], NoPlant)
        .expect_err("refused");
    assert_eq!((e.entry, e.at, e.name.bytes()), (1, 1024, &b"../bad"[..]));
}

impl blaze_sim::Limits for Limits {
    /// The bounds the crate's tests read with; the crate itself ships none (every field is its
    /// caller's choice).
    const SHIPPED: Limits = LIMITS;

    fn draw(tape: &Tape) -> Limits {
        Limits {
            most_path: tape.limit(LIMITS.most_path, 0, "most path"),
            most_meta: tape.limit(LIMITS.most_meta, 0, "most meta"),
            most_entry: tape.limit(LIMITS.most_entry, 0, "most entry"),
            most_total: tape.limit(LIMITS.most_total, 0, "most total"),
            most_entries: tape.limit(LIMITS.most_entries, 0, "most entries"),
            links: *tape.one_of(&[Links::Inside, Links::Refuse], "links"),
        }
    }
}

points! {
    /// Where the bounds' simulation has to get: each bound refusing an archive.
    enum BoundPoint {
        /// `Limits::most_path` refused a path.
        PathOver,
        /// `Limits::most_meta` refused a pax header.
        MetaOver,
        /// `Limits::most_entry` refused a file.
        EntryOver,
        /// `Limits::most_total` refused the files together.
        TotalOver,
        /// `Limits::most_entries` refused a header.
        EntriesOver,
    }
}

/// The bounds' invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum BoundBroken {
    /// An archive past the bound `bound` was read whole.
    Passed { bound: &'static str, limits: Limits },
    /// An archive was refused at a bound it does not pass.
    RefusedEarly { why: Why, entry: u32, limits: Limits },
    /// An archive within its bounds was refused for another reason.
    Refused { why: Why },
    /// An archive within its bounds read back as other entries.
    Differs { read: usize, written: usize },
}

impl fmt::Display for BoundBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundBroken::Passed { bound, limits } => {
                write!(f, "an archive past its {bound} was read, limits {limits:?}")
            }
            BoundBroken::RefusedEarly { why, entry, limits } => {
                write!(f, "header {entry} refused ({why}) within its bounds, limits {limits:?}")
            }
            BoundBroken::Refused { why } => write!(f, "an archive was refused: {why}"),
            BoundBroken::Differs { read, written } => write!(f, "{read} entries read back, {written} written"),
        }
    }
}

/// Every header block of `archive` before its end: its type flag and its size field.
fn headers(archive: &[u8]) -> StdVec<(u8, u64)> {
    let mut out = StdVec::new();
    let mut at = 0usize;
    while at + 512 <= archive.len() {
        let block = &archive[at..at + 512];
        if block.iter().all(|&b| b == 0) {
            break;
        }
        let size = crate::read::number(&block[124..136], Field::Size).expect("our writer's size field");
        out.push((block[156], size));
        at += 512 + (size as usize).div_ceil(512) * 512;
    }
    out
}

/// The bounds' shape: few small entries, so a drawn bound of a few is often past them.
const BOUNDS: Shape = Shape { most_entries: 4, most_depth: 2, most_bytes: 1200 };

/// An archive written here, read under drawn bounds: refused only at a bound it passes.
fn bounds(tape: &Tape) -> Result<(), Broken<BoundBroken>> {
    let limits: Limits = tape.limits();
    let entries = entries(tape, BOUNDS);
    let archive = write_all(&entries, NoPlant);
    let hs = headers(&archive);
    let files = |upto: usize| hs[..upto].iter().filter(|h| h.0 == b'0').map(|h| h.1);
    // A path as the reader sees it: a directory's with its trailing `/`.
    let longest = entries.iter().map(|(p, m, _)| p.len() + usize::from(*m == Make::Dir)).max().unwrap_or(0);
    match read_all(&archive, limits, &[], NoPlant) {
        Ok(got) => {
            let passed = |bound| Err(Broken::Oracle(BoundBroken::Passed { bound, limits }));
            if hs.len() > limits.most_entries as usize {
                return passed("most entries");
            }
            if longest > limits.most_path as usize {
                return passed("most path");
            }
            if hs.iter().any(|h| h.0 == b'x' && h.1 > u64::from(limits.most_meta)) {
                return passed("most meta");
            }
            if files(hs.len()).any(|size| size > limits.most_entry) {
                return passed("most entry");
            }
            if files(hs.len()).sum::<u64>() > limits.most_total {
                return passed("most total");
            }
            let want = expected(&entries);
            if got != want {
                return Err(Broken::Oracle(BoundBroken::Differs { read: got.len(), written: want.len() }));
            }
            Ok(())
        }
        Err(e) => {
            let at = e.entry as usize;
            let header = hs.get(at).copied();
            let legal = match e.why {
                Why::EntriesOver { most } => {
                    tape.reach(BoundPoint::EntriesOver);
                    most == limits.most_entries && hs.len() > most as usize && at == most as usize
                }
                Why::Path(PathWhy::TooLong { len, most }) => {
                    tape.reach(BoundPoint::PathOver);
                    most == limits.most_path && len > most
                }
                Why::MetaOver { size, most } => {
                    tape.reach(BoundPoint::MetaOver);
                    most == limits.most_meta && size > u64::from(most) && header.map(|h| h.1) == Some(size)
                }
                Why::EntryOver { size, most } => {
                    tape.reach(BoundPoint::EntryOver);
                    most == limits.most_entry && size > most && header == Some((b'0', size))
                }
                Why::TotalOver { most } => {
                    tape.reach(BoundPoint::TotalOver);
                    most == limits.most_total && at < hs.len() && files(at + 1).sum::<u64>() > most
                }
                why => return Err(Broken::Oracle(BoundBroken::Refused { why })),
            };
            match legal {
                true => Ok(()),
                false => Err(Broken::Oracle(BoundBroken::RefusedEarly { why: e.why, entry: e.entry, limits })),
            }
        }
    }
}

#[test]
fn each_bound_refuses_at_itself_and_nowhere_else() {
    check(Config::cases(1024).must_reach_points(<BoundPoint as Point>::IDS), bounds);
}
