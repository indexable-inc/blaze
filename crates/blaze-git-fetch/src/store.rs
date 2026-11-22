//! A repository's object store read from its files, one object at a time: the pieces shared by
//! the fetched pack ([`crate::Index`]) and a checkout's `.git/objects` (blaze-build-script's
//! `git` answers, decision D54). Sans-IO: the caller reads the files (a loose object whole, a
//! pack index whole, a pack by ranges) and hands the bytes here.
//!
//! Formats, from git's documentation (`gitformat-pack`, `gitrepository-layout`):
//! - a loose object `objects/ab/cdef..` is the zlib of `<kind> <decimal size>\0<body>`;
//! - a pack index, version 2: `\377tOc`, the version (2), a fan-out of 256 big-endian counts
//!   (entry `b` counts the ids whose first byte is at most `b`), the sorted 20-byte ids, a CRC-32
//!   per id, a 4-byte offset per id (the high bit set: an index into the next table), the 8-byte
//!   offsets, then the pack's and the index's SHA-1s;
//! - a pack entry: a header of type (bits 4-6 of the first byte) and inflated size (a
//!   little-endian varint, 4 bits then 7 per byte), then for an `OFS_DELTA` (6) the distance back
//!   to its base (git's offset encoding, [`crate::pack`]), for a `REF_DELTA` (7) the base's id,
//!   then the zlib of the body or the delta.
//!
//! Every length is checked before it is used; a malformed file is a [`StoreWhy`] with the
//! offset, which the caller pairs with the file it read.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_deflate::ZlibDecoder;
use blaze_own::Vec;

use crate::{
    oid::{Kind, Oid},
    pack::{self, PackWhy},
};

/// Why an object-store file was refused, at which byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreWhy {
    /// The pack index is shorter than its header, fan-out and tables say.
    IdxShort,
    IdxMagic,
    IdxVersion { version: u32 },
    /// A fan-out count smaller than the one before it.
    IdxFanout { byte: u8 },
    IdxOver { count: u32, most: u32 },
    /// An offset naming an 8-byte entry past the table, or past the pack's end.
    IdxOffset { entry: u32 },
    /// A pack entry: its header, type, inflate, size or delta.
    Entry(PackWhy),
    /// A loose object's `<kind> <size>\0` header is not what git writes.
    LooseHeader,
    Loose(blaze_deflate::Error),
    /// The bytes do not hash to the id they were found under.
    Id { want: Oid },
}

/// A version 2 pack index, borrowed.
#[derive(Clone, Copy)]
pub struct Idx<'i> {
    bytes: &'i [u8],
    count: u32,
    large: usize,
}

const FANOUT: usize = 8;
const IDS: usize = FANOUT + 256 * 4;

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl<'i> Idx<'i> {
    /// `bytes` read as a version 2 index of at most `most` objects.
    pub fn parse(bytes: &'i [u8], most: u32) -> Result<Idx<'i>, (u64, StoreWhy)> {
        if bytes.len() < IDS + 40 {
            return Err((0, StoreWhy::IdxShort));
        }
        if bytes[..4] != [0xff, b't', b'O', b'c'] {
            return Err((0, StoreWhy::IdxMagic));
        }
        let version = be32(bytes, 4);
        if version != 2 {
            return Err((4, StoreWhy::IdxVersion { version }));
        }
        let mut last = 0;
        for byte in 0..256usize {
            let count = be32(bytes, FANOUT + byte * 4);
            if count < last {
                return Err(((FANOUT + byte * 4) as u64, StoreWhy::IdxFanout { byte: byte as u8 }));
            }
            last = count;
        }
        if last > most {
            return Err((IDS as u64 - 4, StoreWhy::IdxOver { count: last, most }));
        }
        // `last <= most <= u32::MAX`: 28 bytes per object fits a u64, and is checked against the length.
        let fixed = IDS as u64 + u64::from(last) * 28 + 40;
        if (bytes.len() as u64) < fixed || (bytes.len() as u64 - fixed) % 8 != 0 {
            return Err((0, StoreWhy::IdxShort));
        }
        Ok(Idx { bytes, count: last, large: ((bytes.len() as u64 - fixed) / 8) as usize })
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The `at`-th id in sorted order.
    pub fn id(&self, at: u32) -> &'i [u8] {
        let start = IDS + at as usize * 20;
        &self.bytes[start..start + 20]
    }

    /// Where `oid` sits among the sorted ids: `Ok` found, `Err` where it would go.
    pub fn search(&self, oid: &Oid) -> Result<u32, u32> {
        let first = usize::from(oid.0[0]);
        let mut low = if first == 0 { 0 } else { be32(self.bytes, FANOUT + (first - 1) * 4) };
        let mut high = be32(self.bytes, FANOUT + first * 4);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.id(mid).cmp(&oid.0[..]) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// The pack offset of the `at`-th id, checked to lie before `pack_end` (the trailer's start).
    pub fn offset(&self, at: u32, pack_end: u64) -> Result<u64, (u64, StoreWhy)> {
        let small_at = IDS + self.count as usize * 24 + at as usize * 4;
        let small = be32(self.bytes, small_at);
        let offset = if small & 0x8000_0000 == 0 {
            u64::from(small)
        } else {
            let entry = (small & 0x7fff_ffff) as usize;
            if entry >= self.large {
                return Err((small_at as u64, StoreWhy::IdxOffset { entry: at }));
            }
            let big_at = IDS + self.count as usize * 28 + entry * 8;
            u64::from_be_bytes(self.bytes[big_at..big_at + 8].try_into().expect("8 bytes"))
        };
        if offset < 12 || offset >= pack_end {
            return Err((small_at as u64, StoreWhy::IdxOffset { entry: at }));
        }
        Ok(offset)
    }

    /// Where the entry at `offset` ends: the next entry's start, else `pack_end`. One pass over
    /// the offsets (the reverse index would make it a lookup).
    pub fn entry_end(&self, offset: u64, pack_end: u64) -> Result<u64, (u64, StoreWhy)> {
        let mut end = pack_end;
        for at in 0..self.count {
            let other = self.offset(at, pack_end)?;
            if other > offset && other < end {
                end = other;
            }
        }
        Ok(end)
    }

    /// The most leading hex digits `oid` shares with any other id here.
    pub fn shared_hex(&self, oid: &Oid) -> usize {
        let at = match self.search(oid) {
            Ok(at) => [at.checked_sub(1), at.checked_add(1).filter(|next| *next < self.count)],
            Err(at) => [at.checked_sub(1), Some(at).filter(|next| *next < self.count)],
        };
        at.into_iter().flatten().map(|other| shared_hex(self.id(other), &oid.0)).max().unwrap_or(0)
    }
}

/// The leading hex digits two ids share.
pub fn shared_hex(a: &[u8], b: &[u8]) -> usize {
    for (at, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            return at * 2 + usize::from(x >> 4 == y >> 4);
        }
    }
    a.len().min(b.len()) * 2
}

/// A pack entry's base, when it is a delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    Whole(Kind),
    /// The base starts this many bytes before the entry.
    Ofs { back: u64 },
    Ref { base: Oid },
}

/// A pack entry's header: its base and its inflated size, and where its zlib starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryHead {
    pub base: Base,
    pub size: u64,
    pub data: usize,
}

/// The header of the entry at the start of `window` (the pack from the entry's offset on).
pub fn entry_head(window: &[u8]) -> Result<EntryHead, StoreWhy> {
    let mut pos = 0;
    let (number, size) = pack::entry_header(window, &mut pos).ok_or(StoreWhy::Entry(PackWhy::Header))?;
    let base = match number {
        6 => Base::Ofs { back: pack::ofs_back(window, &mut pos).ok_or(StoreWhy::Entry(PackWhy::Header))? },
        7 => {
            let id = window.get(pos..pos + 20).ok_or(StoreWhy::Entry(PackWhy::Header))?;
            pos += 20;
            Base::Ref { base: Oid(id.try_into().expect("20 bytes")) }
        }
        number => Base::Whole(Kind::of_pack(number).ok_or(StoreWhy::Entry(PackWhy::Type { number }))?),
    };
    Ok(EntryHead { base, size, data: pos })
}

/// The entry's body (or delta) inflated into `out`, refused past `most` bytes.
pub fn inflate_entry<B: Allocator>(window: &[u8], head: &EntryHead, most: u64, out: &mut Vec<u8, B>) -> Result<(), StoreWhy> {
    if head.size > most {
        return Err(StoreWhy::Entry(PackWhy::ObjectOver { size: head.size, most }));
    }
    pack::inflate(window, head.data, head.size, out).map(|_| ()).map_err(StoreWhy::Entry)
}

/// `delta` applied to `base` into `out`, the result refused past `most` bytes.
pub fn apply_delta<B: Allocator>(base: &[u8], delta: &[u8], most: u64, out: &mut Vec<u8, B>) -> Result<(), StoreWhy> {
    out.clear();
    pack::apply(base, delta, most, out, blaze_own::planted::NoPlant).map_err(StoreWhy::Entry)
}

/// A loose object's file inflated into `out` (header and body), refused past `most` bytes:
/// returns its kind and where its body starts. The bytes are checked to hash to `want`.
pub fn loose<A: Allocator + Clone>(file: &[u8], want: &Oid, most: u64, alloc: A, out: &mut Vec<u8, A>) -> Result<(Kind, usize), (u64, StoreWhy)> {
    out.clear();
    let limits = blaze_deflate::Limits { most_out: most.saturating_add(32), most_ratio: blaze_deflate::MAX_RATIO, most_header: 0 };
    let mut zlib = ZlibDecoder::new_in(limits, alloc);
    let fed = zlib.feed(file, out).map_err(|why| (zlib.stream_len(), StoreWhy::Loose(why)))?;
    if !fed.done {
        zlib.finish().map_err(|why| (file.len() as u64, StoreWhy::Loose(why)))?;
    }
    let bad = (0, StoreWhy::LooseHeader);
    let nul = out.iter().take(32).position(|b| *b == 0).ok_or(bad)?;
    let header = core::str::from_utf8(&out[..nul]).map_err(|_| bad)?;
    let (name, size) = header.split_once(' ').ok_or(bad)?;
    let kind = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag].into_iter().find(|kind| kind.name() == name).ok_or(bad)?;
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) || (size.len() > 1 && size.starts_with('0')) {
        return Err(bad);
    }
    let size: u64 = size.parse().map_err(|_| bad)?;
    if size > most {
        return Err((0, StoreWhy::Entry(PackWhy::ObjectOver { size, most })));
    }
    if size != (out.len() - nul - 1) as u64 {
        return Err((0, StoreWhy::Entry(PackWhy::Size { declared: size, got: (out.len() - nul - 1) as u64 })));
    }
    if crate::sha1::sha1(out) != want.0 {
        return Err((0, StoreWhy::Id { want: *want }));
    }
    Ok((kind, nul + 1))
}

/// A commit's fields that `log` and `describe` read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Commit<'c> {
    pub tree: Oid,
    /// The `parent` lines, in order, each 40 hex digits and a newline.
    parents: &'c [u8],
    /// Seconds since the epoch, and the zone as git writes it (`+0130` is 130).
    pub committed: i64,
    pub zone: i32,
}

/// Why a commit object was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitWhy {
    /// A header line before `committer` is not `tree`, `parent` or `author` as git writes it.
    Header,
    /// No `committer` line, or one whose date is not `<seconds> <+|-HHMM>`.
    Committer,
}

impl<'c> Commit<'c> {
    /// A commit object's body read (git's header order: `tree`, `parent`*, `author`,
    /// `committer`; whatever follows is not read).
    pub fn parse(body: &'c [u8]) -> Result<Commit<'c>, (u64, CommitWhy)> {
        let line_at = |at: usize| -> Option<(&'c [u8], usize)> {
            let rest = body.get(at..)?;
            let end = rest.iter().position(|b| *b == b'\n')?;
            Some((&rest[..end], at + end + 1))
        };
        let header = |at: usize| (at as u64, CommitWhy::Header);
        let (line, mut at) = line_at(0).ok_or(header(0))?;
        let tree = line.strip_prefix(b"tree ").and_then(Oid::parse_hex).ok_or(header(0))?;
        let parents_from = at;
        loop {
            let (line, next) = line_at(at).ok_or(header(at))?;
            if !line.starts_with(b"parent ") {
                break;
            }
            Oid::parse_hex(&line[7..]).ok_or(header(at))?;
            at = next;
        }
        let parents = &body[parents_from..at];
        let (line, next) = line_at(at).ok_or(header(at))?;
        if !line.starts_with(b"author ") {
            return Err(header(at));
        }
        let committer_at = next;
        let bad = (committer_at as u64, CommitWhy::Committer);
        let (line, _) = line_at(committer_at).ok_or(bad)?;
        let line = line.strip_prefix(b"committer ").ok_or(bad)?;
        let close = line.iter().rposition(|b| *b == b'>').ok_or(bad)?;
        let date = core::str::from_utf8(&line[close + 1..]).map_err(|_| bad)?;
        let (seconds, zone) = date.trim_start_matches(' ').split_once(' ').ok_or(bad)?;
        if seconds.is_empty() || seconds.len() > 18 || !seconds.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad);
        }
        let zone_bytes = zone.as_bytes();
        if zone_bytes.len() != 5 || !matches!(zone_bytes[0], b'+' | b'-') || !zone_bytes[1..].iter().all(u8::is_ascii_digit) {
            return Err(bad);
        }
        let magnitude: i32 = zone[1..].parse().map_err(|_| bad)?;
        Ok(Commit {
            tree,
            parents,
            committed: seconds.parse().map_err(|_| bad)?,
            zone: if zone_bytes[0] == b'-' { -magnitude } else { magnitude },
        })
    }

    /// The parents, in order.
    pub fn parents(&self) -> impl Iterator<Item = Oid> + 'c {
        self.parents.chunks_exact(48).map(|line| Oid::parse_hex(&line[7..47]).expect("checked by parse"))
    }
}
