// adapted from makepad/relax (MIT), libs/relax/base/src/git/unpack.rs
//! A packfile read whole (version 2 or 3): its trailer checked, every entry inflated and held
//! to the [`Limits`], deltas (by offset and by base id) applied, and each object's id computed.
//! The objects live in one arena in the caller's allocator.
//!
//! From Relax: the entry header's size varint, the offset delta's base distance
//! (`((back + 1) << 7) | low`), the delta opcodes (a copy's size 0 meaning `0x10000`) and
//! resolving in rounds. Changed here: everything is bounded (objects, each object's size, the
//! total, the delta depth, inflate's ratio), the objects sit in one arena instead of a map of
//! vectors, and an offset delta's base must be an entry's exact start.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{AllocFailure, Vec};

use crate::{
    oid::{Kind, Oid},
    plant::{self, Plant},
    sha1::sha1,
};

/// How much a pack may hold: every field is the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub most_objects: u32,
    /// The most bytes of one object or one delta, inflated.
    pub most_object: u64,
    /// The most bytes of all objects, resolved.
    pub most_total: u64,
    /// The most deltas on one object's chain.
    pub most_depth: u16,
}

/// Why a pack was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackWhy {
    /// Shorter than a header and a trailer.
    Short,
    Magic,
    Version { version: u32 },
    /// The trailer is not the SHA-1 of what precedes it: corrupted in transit.
    Checksum,
    ObjectsOver { count: u32, most: u32 },
    /// An entry's header runs past the pack's end or its size past 64 bits.
    Header,
    Type { number: u8 },
    ObjectOver { size: u64, most: u64 },
    TotalOver { most: u64 },
    Inflate(blaze_deflate::Error),
    /// An entry inflated to other than the size its header says.
    Size { declared: u64, got: u64 },
    /// An offset delta's base is not the start of an earlier entry.
    BaseOffset { back: u64 },
    Delta(DeltaWhy),
    /// A reference delta's base is not in the pack (a thin pack, never asked for).
    MissingBase { base: Oid },
    DepthOver { most: u16 },
    /// Bytes between the last entry and the trailer.
    Trailing { extra: u64 },
    Alloc(AllocFailure),
}

/// Why a delta did not apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeltaWhy {
    Truncated,
    /// The delta's base size is not its base's.
    BaseSize { declared: u64, base: u64 },
    /// A copy reaches past its base.
    CopyPast { end: u64, base: u64 },
    /// Opcode 0, reserved.
    Reserved,
    /// The result is not the size the delta declares.
    Result { declared: u64, got: u64 },
}

/// A refused pack: the entry (its index and its offset in the pack) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PackError {
    pub entry: Option<u32>,
    pub at: u64,
    pub why: PackWhy,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.entry {
            Some(entry) => write!(f, "pack entry {entry} at byte {}: ", self.at)?,
            None => write!(f, "pack at byte {}: ", self.at)?,
        }
        match self.why {
            PackWhy::Short => f.write_str("shorter than a pack's header and trailer"),
            PackWhy::Magic => f.write_str("no `PACK` signature"),
            PackWhy::Version { version } => write!(f, "version {version}, not 2 or 3"),
            PackWhy::Checksum => f.write_str("the trailer is not the SHA-1 of the pack"),
            PackWhy::ObjectsOver { count, most } => write!(f, "{count} objects, past the most allowed, {most}"),
            PackWhy::Header => f.write_str("an entry header cut short or too large"),
            PackWhy::Type { number } => write!(f, "entry type {number}, no object or delta"),
            PackWhy::ObjectOver { size, most } => write!(f, "an object of {size} bytes, past the most allowed, {most}"),
            PackWhy::TotalOver { most } => write!(f, "objects past the most bytes allowed, {most}"),
            PackWhy::Inflate(e) => write!(f, "{e}"),
            PackWhy::Size { declared, got } => write!(f, "inflated to {got} bytes, not the {declared} its header says"),
            PackWhy::BaseOffset { back } => write!(f, "an offset delta {back} bytes back, at no earlier entry"),
            PackWhy::Delta(why) => write!(f, "a delta that does not apply: {why:?}"),
            PackWhy::MissingBase { base } => write!(f, "a delta's base {base} is not in the pack"),
            PackWhy::DepthOver { most } => write!(f, "a delta chain longer than the most allowed, {most}"),
            PackWhy::Trailing { extra } => write!(f, "{extra} bytes after the last entry"),
            PackWhy::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for PackError {}

#[derive(Clone, Copy)]
enum Base {
    None(Kind),
    /// The base entry's index.
    Ofs(u32),
    Ref(Oid),
}

#[derive(Clone, Copy)]
struct Entry {
    at: u64,
    base: Base,
    /// The inflated payload in the raw arena: the object, or the delta.
    raw: (usize, usize),
    /// Once resolved: its kind, id, place in the data arena and chain depth.
    done: Option<(Kind, Oid, (usize, usize), u16)>,
}

/// An object of a pack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Object<'a> {
    pub kind: Kind,
    pub oid: Oid,
    pub bytes: &'a [u8],
}

/// A pack read: its objects, found by id.
pub struct Pack<A: Allocator> {
    data: Vec<u8, A>,
    /// Per object: kind, id and range in `data`, in pack order.
    objects: Vec<(Kind, Oid, usize, usize), A>,
    /// Per object: its delta chain's depth (0 for a whole object).
    depths: Vec<u16, A>,
    /// Indices into `objects`, sorted by id.
    by_oid: Vec<u32, A>,
}

/// The most output inflate may make per input byte: DEFLATE's own ceiling is about 1032.
const RATIO: u64 = 1100;

impl<A: Allocator + Clone> Pack<A> {
    /// The pack that is all of `pack`, read under `limits`.
    pub fn read_in(pack: &[u8], limits: Limits, alloc: A) -> Result<Pack<A>, PackError> {
        let fail = |at: u64, entry: Option<u32>, why| PackError { entry, at, why };
        if pack.len() < 32 {
            return Err(fail(0, None, PackWhy::Short));
        }
        if &pack[..4] != b"PACK" {
            return Err(fail(0, None, PackWhy::Magic));
        }
        let version = be32(&pack[4..8]);
        if version != 2 && version != 3 {
            return Err(fail(4, None, PackWhy::Version { version }));
        }
        let end = pack.len() - 20;
        if sha1(&pack[..end]) != pack[end..] && !plant::on(Plant::TrailerUnchecked) {
            return Err(fail(end as u64, None, PackWhy::Checksum));
        }
        let count = be32(&pack[8..12]);
        if count > limits.most_objects {
            return Err(fail(8, None, PackWhy::ObjectsOver { count, most: limits.most_objects }));
        }
        let alloc_failed = |at: u64, e| fail(at, None, PackWhy::Alloc(e));
        let mut entries: Vec<Entry, A> = Vec::new_in(alloc.clone());
        entries.try_reserve(count as usize).map_err(|e| alloc_failed(8, e))?;
        let mut raw = Vec::new_in(alloc.clone());
        let mut pos = 12usize;
        let mut total = 0u64;
        for index in 0..count {
            let at = pos as u64;
            let refuse = |why| fail(at, Some(index), why);
            let (number, size) = entry_header(&pack[..end], &mut pos).ok_or(refuse(PackWhy::Header))?;
            if size > limits.most_object {
                return Err(refuse(PackWhy::ObjectOver { size, most: limits.most_object }));
            }
            let base = match number {
                6 => {
                    let back = ofs_back(&pack[..end], &mut pos).ok_or(refuse(PackWhy::Header))?;
                    let base_at = at.checked_sub(back).filter(|_| back > 0).ok_or(refuse(PackWhy::BaseOffset { back }))?;
                    let found = entries.binary_search_by_key(&base_at, |e| e.at);
                    Base::Ofs(found.map_err(|_| refuse(PackWhy::BaseOffset { back }))? as u32)
                }
                7 => {
                    let id = pack.get(pos..pos + 20).filter(|_| pos + 20 <= end).ok_or(refuse(PackWhy::Header))?;
                    pos += 20;
                    let mut oid = [0u8; 20];
                    oid.copy_from_slice(id);
                    Base::Ref(Oid(oid))
                }
                n => Base::None(Kind::of_pack(n).ok_or(refuse(PackWhy::Type { number: n }))?),
            };
            total += size;
            if total > limits.most_total {
                return Err(refuse(PackWhy::TotalOver { most: limits.most_total }));
            }
            let start = raw.len();
            let bound = blaze_deflate::Limits { most_out: size, most_ratio: RATIO, most_header: 0 };
            let used = blaze_deflate::unzlib_prefix(&pack[pos..end], bound, alloc.clone(), &mut raw)
                .map_err(|e| refuse(PackWhy::Inflate(e)))?;
            let got = (raw.len() - start) as u64;
            if got != size {
                return Err(refuse(PackWhy::Size { declared: size, got }));
            }
            pos += used;
            entries.push(Entry { at, base, raw: (start, raw.len()), done: None });
        }
        if pos != end {
            return Err(fail(pos as u64, None, PackWhy::Trailing { extra: (end - pos) as u64 }));
        }
        let mut pack = Pack {
            data: Vec::new_in(alloc.clone()),
            objects: Vec::new_in(alloc.clone()),
            depths: Vec::new_in(alloc.clone()),
            by_oid: Vec::new_in(alloc.clone()),
        };
        pack.resolve(&mut entries, &raw, limits, &alloc)?;
        Ok(pack)
    }

    /// Every entry resolved, in rounds: one whose base is not yet resolved waits for the next.
    fn resolve(&mut self, entries: &mut [Entry], raw: &[u8], limits: Limits, alloc: &A) -> Result<(), PackError> {
        let mut scratch = Vec::new_in(alloc.clone());
        let mut left = entries.len();
        let mut rounds = 0u32;
        let mut total = 0u64;
        while left > 0 {
            self.index()?;
            let before = left;
            for index in 0..entries.len() {
                let entry = entries[index];
                if entry.done.is_some() {
                    continue;
                }
                let refuse = |why| PackError { entry: Some(index as u32), at: entry.at, why };
                let payload = &raw[entry.raw.0..entry.raw.1];
                let base = match entry.base {
                    Base::None(kind) => {
                        let start = self.data.len();
                        self.data.try_extend_from_slice(payload).map_err(|e| refuse(PackWhy::Alloc(e)))?;
                        Some((kind, start, self.data.len(), 0))
                    }
                    Base::Ofs(base) => entries[base as usize].done.map(|(kind, _, (s, e), depth)| (kind, s, e, depth + 1)),
                    Base::Ref(oid) => self.find(&oid).map(|i| {
                        let (kind, _, s, e) = self.objects[i];
                        (kind, s, e, self.depths[i] + 1)
                    }),
                };
                let Some((kind, s, e, depth)) = base else { continue };
                if depth > limits.most_depth {
                    return Err(refuse(PackWhy::DepthOver { most: limits.most_depth }));
                }
                let range = match entry.base {
                    Base::None(_) => (s, e),
                    _ => {
                        scratch.clear();
                        apply(&self.data[s..e], payload, limits.most_object, &mut scratch).map_err(refuse)?;
                        total += scratch.len() as u64;
                        if total > limits.most_total {
                            return Err(refuse(PackWhy::TotalOver { most: limits.most_total }));
                        }
                        let start = self.data.len();
                        self.data.try_extend_from_slice(&scratch).map_err(|e| refuse(PackWhy::Alloc(e)))?;
                        (start, self.data.len())
                    }
                };
                let oid = Oid::of(kind, &self.data[range.0..range.1]);
                entries[index].done = Some((kind, oid, range, depth));
                self.objects
                    .try_push((kind, oid, range.0, range.1))
                    .map_err(|refused| refuse(PackWhy::Alloc(refused.why)))?;
                self.depths.try_push(depth).map_err(|refused| refuse(PackWhy::Alloc(refused.why)))?;
                left -= 1;
            }
            if left == before {
                // Nothing moved: some reference delta's base is not in the pack (every chain
                // that is stuck ends at one, since an offset delta's base is an entry).
                let stuck = entries.iter().enumerate().find(|(_, e)| e.done.is_none() && matches!(e.base, Base::Ref(_)));
                let error = match stuck {
                    Some((index, &Entry { at, base: Base::Ref(base), .. })) => {
                        PackError { entry: Some(index as u32), at, why: PackWhy::MissingBase { base } }
                    }
                    _ => PackError { entry: None, at: 0, why: PackWhy::DepthOver { most: limits.most_depth } },
                };
                return Err(error);
            }
            rounds += 1;
            if rounds > u32::from(limits.most_depth) + 1 {
                return Err(PackError { entry: None, at: 0, why: PackWhy::DepthOver { most: limits.most_depth } });
            }
        }
        self.index()
    }

    /// `by_oid` rebuilt over the objects resolved so far.
    fn index(&mut self) -> Result<(), PackError> {
        let objects = &self.objects;
        self.by_oid.clear();
        let fail = |e| PackError { entry: None, at: 0, why: PackWhy::Alloc(e) };
        self.by_oid.try_reserve(objects.len()).map_err(fail)?;
        for i in 0..objects.len() {
            self.by_oid.push(i as u32);
        }
        self.by_oid.sort_unstable_by_key(|&i| objects[i as usize].1);
        Ok(())
    }
}

impl<A: Allocator> Pack<A> {
    fn find(&self, oid: &Oid) -> Option<usize> {
        let at = self.by_oid.binary_search_by_key(oid, |&i| self.objects[i as usize].1).ok()?;
        Some(self.by_oid[at] as usize)
    }

    /// The object `oid`, if the pack holds it.
    pub fn get(&self, oid: &Oid) -> Option<Object<'_>> {
        self.find(oid).map(|i| self.object(i))
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Every object, in the order resolved.
    pub fn objects(&self) -> impl Iterator<Item = Object<'_>> {
        (0..self.objects.len()).map(|i| self.object(i))
    }

    fn object(&self, i: usize) -> Object<'_> {
        let (kind, oid, s, e) = self.objects[i];
        Object { kind, oid, bytes: &self.data[s..e] }
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// An entry's type and inflated size.
fn entry_header(pack: &[u8], pos: &mut usize) -> Option<(u8, u64)> {
    let mut byte = *pack.get(*pos)?;
    *pos += 1;
    let number = (byte >> 4) & 7;
    let mut size = u64::from(byte & 15);
    let mut shift = 4u32;
    while byte & 0x80 != 0 {
        byte = *pack.get(*pos)?;
        *pos += 1;
        if shift > 57 {
            return None;
        }
        size |= u64::from(byte & 0x7f) << shift;
        shift += 7;
    }
    Some((number, size))
}

/// An offset delta's distance back to its base.
fn ofs_back(pack: &[u8], pos: &mut usize) -> Option<u64> {
    let mut byte = *pack.get(*pos)?;
    *pos += 1;
    let mut back = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *pack.get(*pos)?;
        *pos += 1;
        back = back.checked_add(1)?.checked_mul(128)? | u64::from(byte & 0x7f);
    }
    Some(back)
}

/// A delta's size varint.
fn delta_size(delta: &[u8], pos: &mut usize) -> Result<u64, PackWhy> {
    let mut size = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *delta.get(*pos).ok_or(PackWhy::Delta(DeltaWhy::Truncated))?;
        *pos += 1;
        if shift > 63 {
            return Err(PackWhy::Delta(DeltaWhy::Truncated));
        }
        size |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(size);
        }
    }
}

/// `delta` applied to `base`, into `out`.
pub(crate) fn apply<B: Allocator>(base: &[u8], delta: &[u8], most: u64, out: &mut Vec<u8, B>) -> Result<(), PackWhy> {
    let delta_why = PackWhy::Delta;
    let mut pos = 0;
    let declared = delta_size(delta, &mut pos)?;
    if declared != base.len() as u64 {
        return Err(delta_why(DeltaWhy::BaseSize { declared, base: base.len() as u64 }));
    }
    let size = delta_size(delta, &mut pos)?;
    if size > most {
        return Err(PackWhy::ObjectOver { size, most });
    }
    out.try_reserve(size as usize).map_err(PackWhy::Alloc)?;
    let byte = |pos: &mut usize| -> Result<u64, PackWhy> {
        let b = *delta.get(*pos).ok_or(delta_why(DeltaWhy::Truncated))?;
        *pos += 1;
        Ok(u64::from(b))
    };
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0u64;
            let mut len = 0u64;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    offset |= byte(&mut pos)? << (8 * i);
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= byte(&mut pos)? << (8 * i);
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            if plant::on(Plant::CopyOffsetOffByOne) && offset > 0 && offset + len < base.len() as u64 {
                offset += 1;
            }
            let end = offset + len;
            if end > base.len() as u64 {
                return Err(delta_why(DeltaWhy::CopyPast { end, base: base.len() as u64 }));
            }
            if out.len() as u64 + len > size {
                return Err(delta_why(DeltaWhy::Result { declared: size, got: out.len() as u64 + len }));
            }
            out.extend_from_slice(&base[offset as usize..end as usize]);
        } else if op > 0 {
            let insert = delta.get(pos..pos + usize::from(op)).ok_or(delta_why(DeltaWhy::Truncated))?;
            if out.len() as u64 + insert.len() as u64 > size {
                return Err(delta_why(DeltaWhy::Result { declared: size, got: out.len() as u64 + insert.len() as u64 }));
            }
            out.extend_from_slice(insert);
            pos += usize::from(op);
        } else {
            return Err(delta_why(DeltaWhy::Reserved));
        }
    }
    if out.len() as u64 != size {
        return Err(delta_why(DeltaWhy::Result { declared: size, got: out.len() as u64 }));
    }
    Ok(())
}
