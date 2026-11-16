//! The crate's promises, each with its oracle and a planted bug it catches:
//!
//! - read of write is the entries written, for any split of the input, and agrees with the
//!   `tar` crate (a dev-dependency) in both directions;
//! - the writer's bytes are a function of the entries;
//! - every escape is refused, naming its entry.

use std::{alloc::Global, borrow::ToOwned, format, string::String, vec::Vec as StdVec};

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::{
    Error, Event, Kind, Limits, Links, Make, PathWhy, Reader, Why, Writer,
    plant::{self, Plant},
};

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
fn read_all(archive: &[u8], limits: Limits, splits: &[usize]) -> Result<StdVec<Got>, Error> {
    let mut reader = Reader::new_in(limits, Global);
    let mut got: StdVec<Got> = StdVec::new();
    let mut at = 0;
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

/// Entries to write: files at depths and lengths that need ustar's prefix and pax, directories,
/// and links that stay inside.
fn entries(tape: &Tape) -> StdVec<(String, Make<'static>, StdVec<u8>)> {
    let mut out = StdVec::new();
    let count = tape.int_in(0usize..=12, "entries");
    for i in 0..count {
        let depth = tape.int_in(0usize..=4, "depth");
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
            1 => out.push((path, Make::File { exec: true }, tape.bytes(0..=3000, "bytes"))),
            _ => out.push((path, Make::File { exec: false }, tape.bytes(0..=3000, "bytes"))),
        }
    }
    out
}

fn write_all(entries: &[(String, Make<'_>, StdVec<u8>)]) -> Vec<u8, Global> {
    let mut w = Writer::new();
    let mut out = Vec::new_in(Global);
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

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let entries = entries(tape);
    let archive = write_all(&entries);
    if archive[..] != write_all(&entries)[..] {
        return Err(Broke::Oracle(String::from("the writer's bytes differ between runs")));
    }
    let splits = tape.vec(0..=8, "splits", |t| t.int_in(0usize..=2000, "split"));
    let got = read_all(&archive, LIMITS, &splits).map_err(|e| Broke::Oracle(format!("round trip refused: {e}")))?;
    let want = expected(&entries);
    if got != want {
        return Err(Broke::Oracle(format!("round trip differs: {} entries read, {} written", got.len(), want.len())));
    }
    // The reference reads ours the same.
    let mut theirs = tar::Archive::new(&archive[..]);
    let mut n = 0;
    for entry in theirs.entries().map_err(|e| Broke::Oracle(format!("the reference: {e}")))? {
        let mut entry = entry.map_err(|e| Broke::Oracle(format!("the reference: {e}")))?;
        let path = entry.path().map_err(|e| Broke::Oracle(format!("the reference: {e}")))?;
        let path = path.to_str().unwrap_or("").trim_end_matches('/').to_owned();
        let mut bytes = StdVec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(|e| Broke::Oracle(format!("{e}")))?;
        if want.get(n).is_none_or(|w| w.path != path || w.bytes != bytes) {
            return Err(Broke::Oracle(format!("the reference reads entry {n} as `{path}`")));
        }
        n += 1;
    }
    if n != want.len() {
        return Err(Broke::Oracle(format!("the reference reads {n} entries, not {}", want.len())));
    }
    Ok(())
}

#[test]
fn read_of_write_is_the_entries() {
    check(Config::cases(128), round_trip);
}

#[test]
fn the_oracle_catches_a_short_size_field() {
    control(Config::cases(128), Plant::Clean, Plant::SizeShort, "round trip", |tape, p| {
        plant::with(p, || round_trip(tape))
    });
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
    let got = read_all(&archive, LIMITS, &[]).expect("read");
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
    match read_all(&archive(parts), limits, &[]) {
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
    let got = read_all(&archive(&[raw(b'2', b"a/b/l", b"../c", b"")]), LIMITS, &[]).expect("inside");
    assert_eq!(got[0].what, "symlink a/c");
}

#[test]
fn the_oracle_catches_unchecked_parents_and_links() {
    let hostile = || {
        [
            StdVec::from([raw(b'0', b"../x", b"", b"x")]),
            StdVec::from([raw(b'2', b"l", b"d", b""), raw(b'0', b"l/x", b"", b"x")]),
        ]
    };
    let caught = |tape: &Tape, p: Plant| {
        plant::with(p, || {
            let all = hostile();
            let parts = &all[tape.index(all.len(), "archive")];
            match read_all(&archive(parts), LIMITS, &[]) {
                Err(_) => Ok(()),
                Ok(_) => Err(Broke::Oracle(String::from("escape not refused"))),
            }
        })
    };
    control(Config::cases(32), Plant::Clean, Plant::ParentUnchecked, "escape not refused", &caught);
    control(Config::cases(32), Plant::Clean, Plant::ThroughLinkUnchecked, "escape not refused", &caught);
}

#[test]
fn format_faults_are_refused() {
    let mut bad = raw(b'0', b"f", b"", b"x");
    bad[0] = b'g';
    assert!(matches!(refused(&[bad], LIMITS), Why::Checksum { .. }));
    // No end blocks: truncated.
    let only = raw(b'0', b"f", b"", b"x");
    assert_eq!(read_all(&only, LIMITS, &[]).map_err(|e| e.why), Err(Why::Truncated));
    // One zero block, then a header.
    let mut half = StdVec::from([0u8; 512]);
    half.extend(raw(b'0', b"f", b"", b"x"));
    assert_eq!(read_all(&half, LIMITS, &[]).map_err(|e| e.why), Err(Why::EndBlock));
    let small = Limits { most_entry: 2, most_meta: 8, ..LIMITS };
    assert_eq!(refused(&[raw(b'0', b"f", b"", b"xyz")], small), Why::EntryOver { size: 3, most: 2 });
    assert_eq!(refused(&[raw(b'L', b"././@LongLink", b"", b"0123456789\0")], small), Why::MetaOver { size: 11, most: 8 });
    // The error names its entry: the second, at byte 1024.
    let e = read_all(&archive(&[raw(b'0', b"ok", b"", b"x"), raw(b'0', b"../bad", b"", b"")]), LIMITS, &[])
        .expect_err("refused");
    assert_eq!((e.entry, e.at, e.name.bytes()), (1, 1024, &b"../bad"[..]));
}
