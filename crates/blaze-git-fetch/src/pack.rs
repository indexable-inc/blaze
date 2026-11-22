// adapted from makepad/relax (MIT), libs/relax/base/src/git/unpack.rs
//! A packfile (version 2 or 3) indexed in place and read one object at a time.
//!
//! [`Index::new_in`] borrows the pack's bytes and checks its trailer, then walks it twice:
//!
//! 1. Every entry in order: its header, its base (an offset delta's must be an entry's exact
//!    start), and its payload inflated into one reused buffer to learn where the next entry
//!    starts and how large it is. A whole object's id is its bytes' SHA-1, computed there.
//! 2. Every delta, in rounds (one whose base is not resolved yet waits for the next): its chain
//!    walked back to a whole object or a cached base, applied forward, and its id computed from
//!    the result.
//!
//! So every object's id is computed from its bytes before any lookup can find it: an id the
//! server only states (a reference delta's base) names nothing until an object hashes to it.
//! What is kept is a row per object (offset, kind, size, id, base: [`Index::row_bytes`] each)
//! and a base cache bounded in bytes ([`Limits::most_cache`]); no object is held once read.
//!
//! [`Index::read`] then inflates one object straight into the caller's slice (a whole object)
//! or resolves its chain through the cache (a delta), and checks the bytes hash to the object's
//! id before handing them over: a read is a second computation of the same bytes, and the id
//! binds them.
//!
//! From Relax: the entry header's size varint, the offset delta's base distance
//! (`((back + 1) << 7) | low`), the delta opcodes (a copy's size 0 meaning `0x10000`) and
//! resolving in rounds. Changed here: everything is bounded (objects, each object's size, the
//! total, the delta depth, the cache, inflate's ratio), nothing is held but the rows and the
//! cache, and an offset delta's base must be an entry's exact start.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_deflate::ZlibInto;
use blaze_own::{
    AllocFailure, Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    oid::{Kind, Oid},
    plant::GitFetchPlant,
    sha1::sha1,
};

/// How much a pack may hold, and the cache that reads it: every field is the caller's choice,
/// [`Limits::SHIPPED`] the values a real repository is read under (D165: a simulation draws
/// each down to its smallest legal value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most entries of one pack.
    pub most_objects: u32,
    /// The most bytes of one object or one delta, inflated.
    pub most_object: u64,
    /// The most bytes of all objects, resolved.
    pub most_total: u64,
    /// The most deltas on one object's chain.
    pub most_depth: u16,
    /// The most bytes of resolved bases kept to read deltas against (an object larger is not
    /// kept). The index's own memory is its rows, three buffers of the largest object read, and
    /// this.
    pub most_cache: u64,
}

impl Limits {
    /// For real repositories: at most 2,000,000 objects, one of at most 512 MiB, 2 GiB of them
    /// in all, delta chains at most 4096 deep, read through a base cache of at most 32 MiB
    /// (blaze-fetch's `Limits::REAL` names the same values).
    pub const SHIPPED: Limits = Limits {
        most_objects: 2_000_000,
        most_object: 512 << 20,
        most_total: 2 << 30,
        most_depth: 4096,
        most_cache: 32 << 20,
    };
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
    /// An object read again whose bytes do not hash to the id it was indexed under.
    Id { want: Oid },
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
            PackWhy::Id { want } => write!(f, "read again, its bytes do not hash to its id {want}"),
            PackWhy::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for PackError {}

/// No base yet: a whole object's, or a reference delta's before its base is found.
const NO_BASE: u32 = u32::MAX;

#[derive(Clone, Copy)]
enum Base {
    Whole,
    /// An offset delta: its base entry is known from the start.
    Ofs,
    /// A reference delta: its base is the entry that hashes to this id, once one does.
    Ref(Oid),
}

/// One entry of the pack: where it is, and once resolved, what it is.
#[derive(Clone, Copy)]
struct Entry {
    /// The entry's header.
    at: u64,
    /// Its zlib stream.
    data: u64,
    /// Its payload inflated: the object's bytes, or the delta's.
    raw: u64,
    base: Base,
    /// The base entry's index ([`NO_BASE`] for a whole object, or a reference delta not yet
    /// resolved).
    base_at: u32,
    /// The object's size: a whole object's payload, a delta's declared result.
    size: u64,
    /// Once resolved: its kind and its id, computed from its bytes.
    kind: Option<Kind>,
    oid: Oid,
    depth: u16,
}

/// An object the index holds: where, of what kind, how large, and its id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    /// Its entry's index: what [`Index::read`] takes.
    pub at: u32,
    pub kind: Kind,
    pub size: u64,
    pub oid: Oid,
}

/// How the base cache did: a read of a delta starts from a cached base (a hit) or inflates
/// its chain's whole object from the pack again (a miss).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// The most bytes the cache held at once.
    pub held_most: u64,
    /// Bytes inflated from the pack after indexing began, deltas and bases again included.
    pub inflated: u64,
}

/// The most bases cached at once: the cache is searched in turn.
const SLOTS: usize = 64;

struct Slot<A: Allocator> {
    entry: u32,
    used: u64,
    bytes: Vec<u8, A>,
}

/// A pack indexed in place, as shipped: no plant (see the module).
pub type Index<'p, A> = PlantedIndex<'p, A, NoPlant>;

/// A pack indexed in place, its plant seam the type parameter `P` (D165, rule 78): [`Index`]
/// (`P = NoPlant`) where it ships, a [`GitFetchPlant`] under a control. See the module.
pub struct PlantedIndex<'p, A: Allocator + Clone, P: Planted<GitFetchPlant>> {
    /// The planted bug, if any: asked where each would be.
    plant: P,
    /// The pack without its trailer.
    pack: &'p [u8],
    limits: Limits,
    entries: Vec<Entry, A>,
    /// Indices of resolved entries, sorted by id.
    by_oid: Vec<u32, A>,
    slots: Vec<Slot<A>, A>,
    held: u64,
    tick: u64,
    stats: CacheStats,
    /// A chain being resolved, and three buffers: the object so far, the next, a delta.
    chain: Vec<u32, A>,
    now: Vec<u8, A>,
    next: Vec<u8, A>,
    delta: Vec<u8, A>,
    alloc: A,
}

impl<'p, A: Allocator + Clone> PlantedIndex<'p, A, NoPlant> {
    /// The index of the pack that is all of `pack`, under `limits`.
    pub fn new_in(pack: &'p [u8], limits: Limits, alloc: A) -> Result<Index<'p, A>, PackError> {
        PlantedIndex::new_planted(pack, limits, alloc, NoPlant)
    }
}

impl<'p, A: Allocator + Clone, P: Planted<GitFetchPlant>> PlantedIndex<'p, A, P> {
    /// [`Index::new_in`] with its plant seam: `plant` is [`NoPlant`] where it ships and a
    /// [`GitFetchPlant`] under a control (D165).
    pub fn new_planted(pack: &'p [u8], limits: Limits, alloc: A, plant: P) -> Result<PlantedIndex<'p, A, P>, PackError> {
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
        if sha1(&pack[..end]) != pack[end..] && !plant.has(GitFetchPlant::TrailerUnchecked) {
            return Err(fail(end as u64, None, PackWhy::Checksum));
        }
        let count = be32(&pack[8..12]);
        if count > limits.most_objects {
            return Err(fail(8, None, PackWhy::ObjectsOver { count, most: limits.most_objects }));
        }
        let mut index = PlantedIndex {
            plant,
            pack: &pack[..end],
            limits,
            entries: Vec::new_in(alloc.clone()),
            by_oid: Vec::new_in(alloc.clone()),
            slots: Vec::new_in(alloc.clone()),
            held: 0,
            tick: 0,
            stats: CacheStats::default(),
            chain: Vec::new_in(alloc.clone()),
            now: Vec::new_in(alloc.clone()),
            next: Vec::new_in(alloc.clone()),
            delta: Vec::new_in(alloc.clone()),
            alloc,
        };
        index.entries.try_reserve_exact(count as usize).map_err(|e| fail(8, None, PackWhy::Alloc(e)))?;
        let total = index.walk(count)?;
        index.resolve_all(total)?;
        index.stats.inflated = 0;
        Ok(index)
    }

    /// Pass 1: every entry's header, base and payload size; a whole object's id. Returns the
    /// bytes of whole objects.
    fn walk(&mut self, count: u32) -> Result<u64, PackError> {
        let pack = self.pack;
        let limits = self.limits;
        let mut pos = 12usize;
        let mut total = 0u64;
        for index in 0..count {
            let at = pos as u64;
            let refuse = |why| PackError { entry: Some(index), at, why };
            let (number, raw) = entry_header(pack, &mut pos).ok_or(refuse(PackWhy::Header))?;
            if raw > limits.most_object {
                return Err(refuse(PackWhy::ObjectOver { size: raw, most: limits.most_object }));
            }
            let (base, base_at) = match number {
                6 => {
                    let back = ofs_back(pack, &mut pos).ok_or(refuse(PackWhy::Header))?;
                    let base_at = at.checked_sub(back).filter(|_| back > 0).ok_or(refuse(PackWhy::BaseOffset { back }))?;
                    let found = self.entries.binary_search_by_key(&base_at, |e| e.at);
                    (Base::Ofs, found.map_err(|_| refuse(PackWhy::BaseOffset { back }))? as u32)
                }
                7 => {
                    let id = pack.get(pos..pos + 20).ok_or(refuse(PackWhy::Header))?;
                    pos += 20;
                    let mut oid = [0u8; 20];
                    oid.copy_from_slice(id);
                    (Base::Ref(Oid(oid)), NO_BASE)
                }
                n => {
                    Kind::of_pack(n).ok_or(refuse(PackWhy::Type { number: n }))?;
                    (Base::Whole, NO_BASE)
                }
            };
            let used = inflate(pack, pos, raw, &mut self.now).map_err(refuse)?;
            let (kind, size, oid) = match base {
                Base::Whole => {
                    let kind = Kind::of_pack(number).expect("checked above");
                    total += raw;
                    if total > limits.most_total {
                        return Err(refuse(PackWhy::TotalOver { most: limits.most_total }));
                    }
                    (Some(kind), raw, Oid::of(kind, &self.now))
                }
                Base::Ofs | Base::Ref(_) => {
                    let mut p = 0;
                    delta_size(&self.now, &mut p).map_err(refuse)?;
                    let size = delta_size(&self.now, &mut p).map_err(refuse)?;
                    if size > limits.most_object {
                        return Err(refuse(PackWhy::ObjectOver { size, most: limits.most_object }));
                    }
                    (None, size, Oid([0; 20]))
                }
            };
            let entry = Entry { at, data: pos as u64, raw, base, base_at, size, kind, oid, depth: 0 };
            self.entries.try_push(entry).map_err(|refused| refuse(PackWhy::Alloc(refused.why)))?;
            pos += used;
        }
        if pos != pack.len() {
            return Err(PackError { entry: None, at: pos as u64, why: PackWhy::Trailing { extra: (pack.len() - pos) as u64 } });
        }
        Ok(total)
    }

    /// Pass 2: every delta resolved, in rounds, its id computed from its bytes.
    fn resolve_all(&mut self, mut total: u64) -> Result<(), PackError> {
        let limits = self.limits;
        let mut left = self.entries.iter().filter(|e| e.kind.is_none()).count();
        let mut rounds = 0u32;
        while left > 0 {
            self.sort_ids()?;
            let before = left;
            for index in 0..self.entries.len() {
                let entry = self.entries[index];
                if entry.kind.is_some() {
                    continue;
                }
                let refuse = |why| PackError { entry: Some(index as u32), at: entry.at, why };
                let base = match entry.base {
                    Base::Ofs if self.entries[entry.base_at as usize].kind.is_some() => entry.base_at,
                    Base::Ref(oid) => match self.find_at(&oid) {
                        Some(base) => base,
                        None => continue,
                    },
                    _ => continue,
                };
                let base_entry = self.entries[base as usize];
                let depth = base_entry.depth + 1;
                if depth > limits.most_depth {
                    return Err(refuse(PackWhy::DepthOver { most: limits.most_depth }));
                }
                self.entries[index].base_at = base;
                self.resolve(index as u32).map_err(refuse)?;
                total += self.now.len() as u64;
                if total > limits.most_total {
                    return Err(refuse(PackWhy::TotalOver { most: limits.most_total }));
                }
                let kind = base_entry.kind.expect("a resolved base");
                let entry = &mut self.entries[index];
                entry.kind = Some(kind);
                entry.oid = Oid::of(kind, &self.now);
                entry.depth = depth;
                left -= 1;
            }
            if left == before {
                // Nothing moved: some reference delta's base is not in the pack (every chain
                // that is stuck ends at one, since an offset delta's base is an entry).
                let stuck = self.entries.iter().enumerate().find(|(_, e)| e.kind.is_none() && matches!(e.base, Base::Ref(_)));
                return Err(match stuck {
                    Some((index, &Entry { at, base: Base::Ref(base), .. })) => {
                        PackError { entry: Some(index as u32), at, why: PackWhy::MissingBase { base } }
                    }
                    _ => PackError { entry: None, at: 0, why: PackWhy::DepthOver { most: limits.most_depth } },
                });
            }
            rounds += 1;
            if rounds > u32::from(limits.most_depth) + 1 {
                return Err(PackError { entry: None, at: 0, why: PackWhy::DepthOver { most: limits.most_depth } });
            }
        }
        self.sort_ids()
    }

    /// `by_oid` rebuilt over the entries resolved so far.
    fn sort_ids(&mut self) -> Result<(), PackError> {
        let entries = &self.entries;
        self.by_oid.clear();
        let fail = |e| PackError { entry: None, at: 0, why: PackWhy::Alloc(e) };
        self.by_oid.try_reserve(entries.len()).map_err(fail)?;
        for (at, entry) in entries.iter().enumerate() {
            if entry.kind.is_some() {
                self.by_oid.push(at as u32);
            }
        }
        self.by_oid.sort_unstable_by_key(|&at| entries[at as usize].oid);
        Ok(())
    }

    /// The object `at` resolved into `self.now`: its chain walked back to a cached base or a
    /// whole object, then each delta applied forward, each result kept in the cache.
    fn resolve(&mut self, at: u32) -> Result<(), PackWhy> {
        self.chain.clear();
        let mut cur = at;
        loop {
            if let Some(slot) = self.cached(cur) {
                self.now.clear();
                self.now.try_extend_from_slice(&self.slots[slot].bytes).map_err(PackWhy::Alloc)?;
                self.stats.hits += 1;
                break;
            }
            let entry = self.entries[cur as usize];
            if let Base::Whole = entry.base {
                inflate(self.pack, entry.data as usize, entry.raw, &mut self.now)?;
                self.stats.inflated += entry.raw;
                self.stats.misses += 1;
                if !self.chain.is_empty() {
                    self.keep(cur)?;
                }
                break;
            }
            if self.chain.len() > usize::from(self.limits.most_depth) {
                return Err(PackWhy::DepthOver { most: self.limits.most_depth });
            }
            self.chain.try_push(cur).map_err(|refused| PackWhy::Alloc(refused.why))?;
            cur = entry.base_at;
        }
        for step in (0..self.chain.len()).rev() {
            let delta_at = self.chain[step];
            let entry = self.entries[delta_at as usize];
            inflate(self.pack, entry.data as usize, entry.raw, &mut self.delta)?;
            self.stats.inflated += entry.raw;
            self.next.clear();
            apply(&self.now, &self.delta, self.limits.most_object, &mut self.next, self.plant)?;
            core::mem::swap(&mut self.now, &mut self.next);
            self.keep(delta_at)?;
        }
        Ok(())
    }

    /// The cache slot holding `entry`, marked used.
    fn cached(&mut self, entry: u32) -> Option<usize> {
        let slot = self.slots.iter().position(|slot| slot.entry == entry)?;
        self.tick += 1;
        self.slots[slot].used = self.tick;
        Some(slot)
    }

    /// `self.now`, the resolved bytes of `entry`, kept as a base: the least recently used
    /// bases go first to make room; one past the cache's bound is not kept.
    fn keep(&mut self, entry: u32) -> Result<(), PackWhy> {
        let len = self.now.len() as u64;
        if len > self.limits.most_cache || self.cached(entry).is_some() {
            return Ok(());
        }
        while self.slots.len() >= SLOTS || self.held + len > self.limits.most_cache {
            let (oldest, _) =
                self.slots.iter().enumerate().min_by_key(|(_, slot)| slot.used).expect("a full cache has a slot");
            let gone = self.slots.swap_remove(oldest);
            self.held -= gone.bytes.len() as u64;
        }
        let mut bytes = Vec::try_with_capacity_in(self.now.len(), self.alloc.clone()).map_err(PackWhy::Alloc)?;
        bytes.extend_from_slice(&self.now);
        self.tick += 1;
        self.slots.try_push(Slot { entry, used: self.tick, bytes }).map_err(|refused| PackWhy::Alloc(refused.why))?;
        self.held += len;
        self.stats.held_most = self.stats.held_most.max(self.held);
        Ok(())
    }

    fn find_at(&self, oid: &Oid) -> Option<u32> {
        let entries = &self.entries;
        match self.by_oid.binary_search_by_key(oid, |&at| entries[at as usize].oid) {
            Ok(found) => Some(self.by_oid[found]),
            // A planted lookup that takes the entry where the id would be: an object accepted
            // without its id matching.
            Err(near) if self.plant.has(GitFetchPlant::NearestId) && near < self.by_oid.len() => Some(self.by_oid[near]),
            Err(_) => None,
        }
    }

    /// The plant this index was made with: the tree walk asks it too.
    pub fn plant(&self) -> P {
        self.plant
    }

    /// The object `oid`, if an object of the pack hashes to it.
    pub fn find(&self, oid: &Oid) -> Option<Found> {
        self.find_at(oid).map(|at| self.found(at))
    }

    fn found(&self, at: u32) -> Found {
        let entry = self.entries[at as usize];
        Found { at, kind: entry.kind.expect("only resolved entries are indexed"), size: entry.size, oid: entry.oid }
    }

    /// Every object, in pack order.
    pub fn objects(&self) -> impl Iterator<Item = Found> + '_ {
        (0..self.entries.len() as u32).map(|at| self.found(at))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes the index keeps per object.
    pub const fn row_bytes() -> usize {
        size_of::<Entry>() + size_of::<u32>()
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// The object `found` (from [`Index::find`]) into `out`, which must be its size: a whole
    /// object inflated straight into it, a delta resolved through the cache. Its bytes are
    /// checked to hash to its id before this returns.
    pub fn read(&mut self, found: Found, out: &mut [u8]) -> Result<(), PackError> {
        let entry = self.entries[found.at as usize];
        let refuse = |why| PackError { entry: Some(found.at), at: entry.at, why };
        if out.len() as u64 != entry.size {
            return Err(refuse(PackWhy::Size { declared: entry.size, got: out.len() as u64 }));
        }
        match (entry.base, self.cached(found.at)) {
            (Base::Whole, None) => {
                inflate_into(&self.pack[entry.data as usize..], out).map_err(refuse)?;
                self.stats.inflated += entry.raw;
            }
            _ => {
                self.resolve(found.at).map_err(refuse)?;
                out.copy_from_slice(&self.now);
            }
        }
        let kind = entry.kind.expect("only resolved entries are found");
        if Oid::of(kind, out) != entry.oid {
            return Err(refuse(PackWhy::Id { want: entry.oid }));
        }
        Ok(())
    }

    /// The object `found` into `out`, cleared first: for a tree or a commit, read whole.
    pub fn read_vec<B: Allocator>(&mut self, found: Found, out: &mut Vec<u8, B>) -> Result<(), PackError> {
        let refuse = |why| PackError { entry: Some(found.at), at: 0, why };
        out.clear();
        let size = usize::try_from(found.size).map_err(|_| refuse(PackWhy::ObjectOver { size: found.size, most: self.limits.most_object }))?;
        out.try_reserve_exact(size).map_err(|e| refuse(PackWhy::Alloc(e)))?;
        out.resize(size, 0);
        self.read(found, out.as_mut_slice())
    }
}

/// The zlib stream at `pack[data..]` inflated into `out`, sized to `raw` first: returns the
/// stream's length.
pub(crate) fn inflate<B: Allocator>(pack: &[u8], data: usize, raw: u64, out: &mut Vec<u8, B>) -> Result<usize, PackWhy> {
    out.clear();
    let size = usize::try_from(raw).map_err(|_| PackWhy::ObjectOver { size: raw, most: usize::MAX as u64 })?;
    out.try_reserve(size).map_err(PackWhy::Alloc)?;
    out.resize(size, 0);
    inflate_into(&pack[data..], out.as_mut_slice())
}

/// The zlib stream at the start of `input` inflated into all of `out`, no byte more or fewer:
/// returns the stream's length.
fn inflate_into(input: &[u8], out: &mut [u8]) -> Result<usize, PackWhy> {
    let bound = blaze_deflate::Limits { most_out: out.len() as u64, most_ratio: blaze_deflate::MAX_RATIO, most_header: 0 };
    let mut zlib = ZlibInto::new(bound);
    let fed = zlib.feed(input, out).map_err(PackWhy::Inflate)?;
    if !fed.done {
        zlib.finish().map_err(PackWhy::Inflate)?;
    }
    if zlib.written() != out.len() as u64 {
        return Err(PackWhy::Size { declared: out.len() as u64, got: zlib.written() });
    }
    Ok(zlib.stream_len() as usize)
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// An entry's type and inflated size.
pub(crate) fn entry_header(pack: &[u8], pos: &mut usize) -> Option<(u8, u64)> {
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
pub(crate) fn ofs_back(pack: &[u8], pos: &mut usize) -> Option<u64> {
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

/// `delta` applied to `base`, into `out`; `plant` is the seam of the copy's plant.
pub(crate) fn apply<B: Allocator>(
    base: &[u8],
    delta: &[u8],
    most: u64,
    out: &mut Vec<u8, B>,
    plant: impl Planted<GitFetchPlant>,
) -> Result<(), PackWhy> {
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
            if plant.has(GitFetchPlant::CopyOffsetOffByOne) && offset > 0 && offset + len < base.len() as u64 {
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
