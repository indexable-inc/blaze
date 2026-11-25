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
use blaze_own::{BTreeSet, Vec};

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

/// A version 1 or version 2 pack index, borrowed.
#[derive(Clone, Copy)]
pub struct Idx<'i> {
    bytes: &'i [u8],
    count: u32,
    large: usize,
    version: u8,
}

const FANOUT: usize = 8;
const IDS: usize = FANOUT + 256 * 4;
const V1_IDS: usize = 256 * 4;

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl<'i> Idx<'i> {
    /// `bytes` read as a version 1 or version 2 pack index of at most `most` objects.
    pub fn parse(bytes: &'i [u8], most: u32) -> Result<Idx<'i>, (u64, StoreWhy)> {
        if bytes.len() >= 8 && bytes.get(..4) == Some(&[0xff, b't', b'O', b'c']) {
            if bytes.len() < IDS + 40 {
                return Err((0, StoreWhy::IdxShort));
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
            return Ok(Idx { bytes, count: last, large: ((bytes.len() as u64 - fixed) / 8) as usize, version: 2 });
        }
        // Version 1 pack index: no 8-byte signature, starts directly with the 256-entry fanout,
        // followed by `count` 24-byte (u32 offset, 20-byte SHA-1) entries and 40-byte trailer.
        if bytes.len() < V1_IDS + 40 {
            return Err((0, StoreWhy::IdxShort));
        }
        let mut last = 0;
        for byte in 0..256usize {
            let count = be32(bytes, byte * 4);
            if count < last {
                let at = u64::try_from(byte * 4).unwrap_or(u64::MAX);
                return Err((at, StoreWhy::IdxFanout { byte: u8::try_from(byte).unwrap_or(u8::MAX) }));
            }
            last = count;
        }
        if last > most {
            return Err((u64::try_from(V1_IDS - 4).unwrap_or(u64::MAX), StoreWhy::IdxOver { count: last, most }));
        }
        let expected = u64::try_from(V1_IDS).unwrap_or(u64::MAX) + u64::from(last) * 24 + 40;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected {
            return Err((0, StoreWhy::IdxMagic));
        }
        Ok(Idx { bytes, count: last, large: 0, version: 1 })
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Verifies the trailing 20-byte SHA-1 checksum over all preceding bytes of this index.
    pub fn verify_checksum(&self) -> Result<(), (u64, StoreWhy)> {
        let Some(body_len) = self.bytes.len().checked_sub(20) else {
            return Err((0, StoreWhy::IdxShort));
        };
        let (Some(body), Some(trailer)) = (self.bytes.get(..body_len), self.bytes.get(body_len..)) else {
            return Err((0, StoreWhy::IdxShort));
        };
        let Ok(want) = <[u8; 20]>::try_from(trailer) else {
            return Err((0, StoreWhy::IdxShort));
        };
        if crate::sha1::sha1(body) != want {
            return Err((u64::try_from(body_len).unwrap_or(u64::MAX), StoreWhy::Id { want: Oid(want) }));
        }
        Ok(())
    }

    /// Verifies that every OID lies in the fanout bucket matching its first byte and that the OID
    /// table is strictly ascending (no unsorted or duplicate entries), guaranteeing binary search
    /// in [`Idx::search`] is sound.
    pub fn verify_ordering(&self) -> Result<(), (u64, StoreWhy)> {
        let fanout = if self.version == 1 { 0 } else { FANOUT };
        for byte in 0..256usize {
            let low = if byte == 0 { 0 } else { be32(self.bytes, fanout + (byte - 1) * 4) };
            let high = be32(self.bytes, fanout + byte * 4);
            let expected = u8::try_from(byte).unwrap_or(u8::MAX);
            let mut prev: Option<&[u8]> = None;
            for at in low..high {
                let id = self.id(at);
                if id.first() != Some(&expected) {
                    let off = u64::try_from(fanout + byte * 4).unwrap_or(u64::MAX);
                    return Err((off, StoreWhy::IdxFanout { byte: expected }));
                }
                if let Some(last) = prev
                    && id <= last
                {
                    let off = u64::try_from(fanout + byte * 4).unwrap_or(u64::MAX);
                    return Err((off, StoreWhy::IdxFanout { byte: expected }));
                }
                prev = Some(id);
            }
        }
        Ok(())
    }

    /// The `at`-th id in sorted order.
    pub fn id(&self, at: u32) -> &'i [u8] {
        if self.version == 1 {
            let start = V1_IDS + usize::try_from(at).unwrap_or(usize::MAX) * 24 + 4;
            return self.bytes.get(start..start + 20).unwrap_or(&[]);
        }
        let start = IDS + at as usize * 20;
        &self.bytes[start..start + 20]
    }

    /// Where `oid` sits among the sorted ids: `Ok` found, `Err` where it would go.
    pub fn search(&self, oid: &Oid) -> Result<u32, u32> {
        let fanout = if self.version == 1 { 0 } else { FANOUT };
        let first = usize::from(oid.0[0]);
        let mut low = if first == 0 { 0 } else { be32(self.bytes, fanout + (first - 1) * 4) };
        let mut high = be32(self.bytes, fanout + first * 4);
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
        if self.version == 1 {
            let off_at = V1_IDS + usize::try_from(at).unwrap_or(usize::MAX) * 24;
            let offset = u64::from(be32(self.bytes, off_at));
            if offset < 12 || offset >= pack_end {
                return Err((u64::try_from(off_at).unwrap_or(u64::MAX), StoreWhy::IdxOffset { entry: at }));
            }
            return Ok(offset);
        }
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

/// Read-only sans-IO access to one git repository's `objects/` store: loose objects and ranged
/// reads into indexed packfiles.
pub trait ObjectSource<A: Allocator> {
    /// Reads loose object `objects/<hex[0..2]>/<hex[2..40]>` into `out` (cleared first).
    /// Returns `Ok(true)` if present, `Ok(false)` if absent.
    fn read_loose(&mut self, oid: &Oid, out: &mut Vec<u8, A>) -> Result<bool, StoreWhy>;

    /// Reads `len` bytes at `offset` from pack `pack_idx` into `out` (cleared first).
    /// Returns `Ok(true)` if all `len` bytes were read, `Ok(false)` if short or absent.
    fn read_pack_range(&mut self, pack_idx: usize, offset: u64, len: u64, out: &mut Vec<u8, A>) -> Result<bool, StoreWhy>;
}

struct PackTable<'i, A: Allocator> {
    idx: Idx<'i>,
    pack_end: u64,
    /// Sorted pack offsets, built lazily on the first object read from this pack so `entry_end`
    /// is O(log n) rather than an O(n) scan per object.
    sorted_offsets: Option<Vec<u64, A>>,
}

impl<'i, A: Allocator + Copy> PackTable<'i, A> {
    fn entry_end(&mut self, offset: u64, alloc: A) -> Result<u64, (u64, StoreWhy)> {
        if self.sorted_offsets.is_none() {
            let count = usize::try_from(self.idx.len()).map_err(|_| (0, StoreWhy::IdxShort))?;
            let mut offsets: Vec<u64, A> = Vec::new_in(alloc);
            offsets.try_reserve(count).map_err(|e| (0, StoreWhy::Entry(PackWhy::Alloc(e))))?;
            for at in 0..self.idx.len() {
                offsets.push(self.idx.offset(at, self.pack_end)?);
            }
            offsets.sort_unstable();
            self.sorted_offsets = Some(offsets);
        }
        let Some(offsets) = self.sorted_offsets.as_ref() else {
            return Ok(self.pack_end);
        };
        let next = offsets.partition_point(|&o| o <= offset);
        Ok(offsets.get(next).copied().unwrap_or(self.pack_end))
    }
}

/// On-demand object reader over a repository's loose objects and all indexed packfiles.
///
/// Only touches `.idx` tables (O(log n) lookup per pack) and ranged slices of reachable objects —
/// never scans or inflates unreachable history. Resolves both `OFS_DELTA` (within one pack) and
/// `REF_DELTA` (across any pack or loose object) iteratively within [`crate::PackLimits`], and
/// verifies every resolved object's SHA-1 against its requested [`Oid`].
pub struct ObjectDb<'i, A: Allocator> {
    packs: Vec<PackTable<'i, A>, A>,
    limits: crate::PackLimits,
    resolved_objects: u32,
    resolved_bytes: u64,
    /// Distinct object IDs already charged against `PackLimits::{most_objects, most_total}` so
    /// multiple tree paths referencing the same blob OID are charged only once (matching the
    /// network pack path), while distinct objects and their delta intermediates remain charged.
    charged_oids: BTreeSet<Oid, A>,
    alloc: A,
}

impl<'i, A: Allocator + Copy> ObjectDb<'i, A> {
    /// Creates an object DB over `packs` (each `(raw .idx bytes, .pack byte length)`).
    pub fn new(
        packs: &[(&'i [u8], u64)],
        limits: crate::PackLimits,
        alloc: A,
    ) -> Result<ObjectDb<'i, A>, (u64, StoreWhy)> {
        let mut parsed: Vec<PackTable<'i, A>, A> = Vec::new_in(alloc);
        parsed.try_reserve(packs.len()).map_err(|e| (0, StoreWhy::Entry(PackWhy::Alloc(e))))?;
        for &(idx_bytes, pack_len) in packs {
            if pack_len < 32 {
                return Err((0, StoreWhy::Entry(PackWhy::Short)));
            }
            let idx = Idx::parse(idx_bytes, limits.most_objects)?;
            idx.verify_checksum()?;
            idx.verify_ordering()?;
            parsed.push(PackTable { idx, pack_end: pack_len - 20, sorted_offsets: None });
        }
        Ok(ObjectDb {
            packs: parsed,
            limits,
            resolved_objects: 0,
            resolved_bytes: 0,
            charged_oids: BTreeSet::new_in(alloc),
            alloc,
        })
    }

    fn charge_object(&mut self, bytes: u64) -> Result<(), (u64, StoreWhy)> {
        let count = self.resolved_objects.saturating_add(1);
        if count > self.limits.most_objects {
            return Err((0, StoreWhy::Entry(PackWhy::ObjectsOver { count, most: self.limits.most_objects })));
        }
        let total = self.resolved_bytes.saturating_add(bytes);
        if total > self.limits.most_total {
            return Err((0, StoreWhy::Entry(PackWhy::TotalOver { most: self.limits.most_total })));
        }
        self.resolved_objects = count;
        self.resolved_bytes = total;
        Ok(())
    }

    fn record_charged_oid(&mut self, oid: Oid) -> Result<(), (u64, StoreWhy)> {
        self.charged_oids.try_insert(oid).map_err(|r| (0, StoreWhy::Entry(PackWhy::Alloc(r.why))))?;
        Ok(())
    }

    fn charge_once(&mut self, oid: Oid, bytes: u64) -> Result<(), (u64, StoreWhy)> {
        if self.charged_oids.contains(&oid) {
            return Ok(());
        }
        self.charge_object(bytes)?;
        self.record_charged_oid(oid)
    }

    /// Reads object `oid` into `out` (cleared first), checking loose objects first and then all
    /// indexed packs. Verifies that the resolved bytes hash to `oid`, and charges each distinct
    /// object (including delta bases and intermediates) once against `PackLimits`. Returns
    /// `Ok(None)` when no loose object or pack index holds `oid` (reading zero packfile bytes).
    pub fn read_object<S: ObjectSource<A>>(
        &mut self,
        oid: &Oid,
        source: &mut S,
        out: &mut Vec<u8, A>,
    ) -> Result<Option<Kind>, (u64, StoreWhy)> {
        out.clear();
        let mut raw = Vec::new_in(self.alloc);
        if source.read_loose(oid, &mut raw).map_err(|why| (0, why))? {
            let mut whole = Vec::new_in(self.alloc);
            let (kind, body) = loose(&raw, oid, self.limits.most_object, self.alloc, &mut whole)?;
            let slice = whole.get(body..).ok_or((0, StoreWhy::LooseHeader))?;
            self.charge_once(*oid, u64::try_from(slice.len()).unwrap_or(u64::MAX))?;
            out.try_extend_from_slice(slice).map_err(|e| (0, StoreWhy::Entry(PackWhy::Alloc(e))))?;
            return Ok(Some(kind));
        }
        let Some((pack_idx, found)) = self.find_in_packs(oid) else {
            return Ok(None);
        };
        let kind = self.read_packed(pack_idx, found, oid, source, out)?;
        Ok(Some(kind))
    }

    fn find_in_packs(&self, oid: &Oid) -> Option<(usize, u32)> {
        for (pack_idx, table) in self.packs.iter().enumerate() {
            if let Ok(found) = table.idx.search(oid) {
                return Some((pack_idx, found));
            }
        }
        None
    }

    fn read_packed<S: ObjectSource<A>>(
        &mut self,
        start_pack: usize,
        found: u32,
        oid: &Oid,
        source: &mut S,
        out: &mut Vec<u8, A>,
    ) -> Result<Kind, (u64, StoreWhy)> {
        let mut cur_pack = start_pack;
        let mut cur_at = {
            let table = self.packs.get(cur_pack).ok_or((0, StoreWhy::IdxShort))?;
            table.idx.offset(found, table.pack_end)?
        };
        let mut window = Vec::new_in(self.alloc);
        let mut chain: Vec<(usize, u64), A> = Vec::new_in(self.alloc);
        let max_window = self.limits.most_object.saturating_mul(2).saturating_add(64);
        let top_charged = self.charged_oids.contains(oid);

        let kind = loop {
            let head = self.read_entry_window(cur_pack, cur_at, max_window, source, &mut window)?;
            match head.base {
                Base::Whole(kind) => {
                    inflate_entry(&window, &head, self.limits.most_object, out).map_err(|why| (cur_at, why))?;
                    if !top_charged {
                        let base_oid = if chain.is_empty() { *oid } else { Oid::of(kind, out) };
                        self.charge_once(base_oid, u64::try_from(out.len()).unwrap_or(u64::MAX))?;
                    }
                    break kind;
                }
                Base::Ofs { back } => {
                    if back == 0 {
                        return Err((cur_at, StoreWhy::Entry(PackWhy::BaseOffset { back })));
                    }
                    if chain.len() >= usize::from(self.limits.most_depth) {
                        return Err((cur_at, StoreWhy::Entry(PackWhy::DepthOver { most: self.limits.most_depth })));
                    }
                    chain.try_push((cur_pack, cur_at)).map_err(|r| (cur_at, StoreWhy::Entry(PackWhy::Alloc(r.why))))?;
                    cur_at = match cur_at.checked_sub(back) {
                        Some(base) if base >= 12 => base,
                        _ => return Err((cur_at, StoreWhy::Entry(PackWhy::BaseOffset { back }))),
                    };
                }
                Base::Ref { base } => {
                    if chain.len() >= usize::from(self.limits.most_depth) {
                        return Err((cur_at, StoreWhy::Entry(PackWhy::DepthOver { most: self.limits.most_depth })));
                    }
                    chain.try_push((cur_pack, cur_at)).map_err(|r| (cur_at, StoreWhy::Entry(PackWhy::Alloc(r.why))))?;
                    // Cross-pack or loose base lookup across the entire object DB.
                    let mut loose_raw = Vec::new_in(self.alloc);
                    if source.read_loose(&base, &mut loose_raw).map_err(|why| (cur_at, why))? {
                        let mut whole = Vec::new_in(self.alloc);
                        let (kind, body) = loose(&loose_raw, &base, self.limits.most_object, self.alloc, &mut whole)?;
                        let slice = whole.get(body..).ok_or((cur_at, StoreWhy::LooseHeader))?;
                        out.clear();
                        out.try_extend_from_slice(slice).map_err(|e| (cur_at, StoreWhy::Entry(PackWhy::Alloc(e))))?;
                        if !top_charged {
                            self.charge_once(base, u64::try_from(out.len()).unwrap_or(u64::MAX))?;
                        }
                        break kind;
                    }
                    let Some((base_pack, base_idx)) = self.find_in_packs(&base) else {
                        return Err((cur_at, StoreWhy::Entry(PackWhy::MissingBase { base })));
                    };
                    let base_at = {
                        let table = self.packs.get(base_pack).ok_or((cur_at, StoreWhy::IdxShort))?;
                        table.idx.offset(base_idx, table.pack_end)?
                    };
                    cur_pack = base_pack;
                    cur_at = base_at;
                }
            }
        };

        let mut delta = Vec::new_in(self.alloc);
        let mut next = Vec::new_in(self.alloc);
        while let Some((delta_pack, delta_at)) = chain.pop() {
            let head = self.read_entry_window(delta_pack, delta_at, max_window, source, &mut window)?;
            delta.clear();
            inflate_entry(&window, &head, self.limits.most_object, &mut delta).map_err(|why| (delta_at, why))?;
            apply_delta(out, &delta, self.limits.most_object, &mut next).map_err(|why| (delta_at, why))?;
            core::mem::swap(out, &mut next);
            if !top_charged {
                let step_oid = if chain.is_empty() { *oid } else { Oid::of(kind, out) };
                self.charge_once(step_oid, u64::try_from(out.len()).unwrap_or(u64::MAX))?;
            }
        }
        if Oid::of(kind, out) != *oid {
            return Err((0, StoreWhy::Id { want: *oid }));
        }
        Ok(kind)
    }

    fn read_entry_window<S: ObjectSource<A>>(
        &mut self,
        pack_idx: usize,
        at: u64,
        max_window: u64,
        source: &mut S,
        window: &mut Vec<u8, A>,
    ) -> Result<EntryHead, (u64, StoreWhy)> {
        let alloc = self.alloc;
        let end = {
            let table = self.packs.get_mut(pack_idx).ok_or((at, StoreWhy::IdxShort))?;
            table.entry_end(at, alloc)?
        };
        let len = end.saturating_sub(at);
        if len == 0 || len > max_window {
            return Err((at, StoreWhy::Entry(PackWhy::ObjectOver { size: len, most: max_window })));
        }
        window.clear();
        if !source.read_pack_range(pack_idx, at, len, window).map_err(|why| (at, why))? {
            return Err((at, StoreWhy::Entry(PackWhy::Header)));
        }
        entry_head(window).map_err(|why| (at, why))
    }
}
