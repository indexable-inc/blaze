//! The symbol table's storage on reserved regions (docs/v2/plan/design/memory.md, "tables,
//! interners and columns"; docs/v2/plan/lanes/mem/backend-link.md).
//!
//! Each of the 64 shards of [`crate::symbols::Symbols`]' three tables (symbol keys, names, ids) keeps its
//! entries in [`RegionVec`]s tagged [`Tag::Link`] and finds them through an [`Index`]: an
//! open-addressing table over a region of its own (huge pages past its first 2 MiB, as it is
//! probed at random), each slot an entry's index beside the high
//! half of its hash, probed linearly from the hash's low bits and kept at most half full. A
//! region grows by committing pages and the index doubles in place, rebuilt from the entries,
//! so no table is ever moved or copied, and interning into a warm image makes no call to the
//! general-purpose allocator however far the tables grow.
//!
//! The bounds are per shard, fixed when the image is made ([`MAX_ENTRIES`],
//! [`MAX_NAME_BYTES`]): address space only, committed as entries arrive. Interning past one
//! stops with the [`MemError`] naming `Tag::Link` (no-fallback), as [`RegionVec::push`] does.
//!
//! The one `unsafe` block is [`name_at`]'s view of a name's bytes as the `&str` they were
//! appended from.

#![deny(unsafe_code)]

use blaze_ids::ContentId;
use blaze_mem::{MemError, RegionVec, Tag};

use crate::symbols::{SHARDS, SymKey, hash_id, hash_key, hash_name};

/// Entries one shard of one table holds at most: 2^20, so 2^26 (67 million) symbols, names or
/// ids per image, past ten times the largest images linked (Chromium's 4 to 5 million
/// symbols). Below the symbol id's 2^26 per shard ([`crate::symbols`]'s `SHIFT`).
pub(crate) const MAX_ENTRIES: usize = 1 << 20;

/// Bytes of names one shard holds at most: 64 MiB, so 4 GiB per image (the largest string
/// tables are a few hundred MiB of mangled names), and every name's end fits its `u32`.
pub(crate) const MAX_NAME_BYTES: usize = 1 << 26;

/// The index's slots for [`MAX_ENTRIES`] at most half full.
const MAX_SLOTS: usize = (2 * MAX_ENTRIES).next_power_of_two();

/// The index's first size: 8 KiB of slots, for 512 entries.
const MIN_SLOTS: usize = 1024;

const _: () = assert!(MAX_ENTRIES < u32::MAX as usize && MAX_NAME_BYTES <= u32::MAX as usize);
const _: () = assert!(MIN_SLOTS.is_power_of_two() && MIN_SLOTS <= MAX_SLOTS);

/// What the image reserves for its symbol table: per shard, keys (20 B) and ids (32 B) with
/// an index each, names' ends (4 B) and bytes with an index; times [`SHARDS`]. 10.5 GiB of
/// address space, none of it committed until used.
#[cfg(test)]
pub(crate) const RESERVED: usize =
    SHARDS * (MAX_ENTRIES * (size_of::<SymKey>() + size_of::<ContentId>() + 4) + MAX_NAME_BYTES + 3 * MAX_SLOTS * 8);

/// One shard of one table: finds a key or adds it, returning its index in the shard.
pub(crate) trait Shard<K>: Send {
    fn get_or_insert(&mut self, hash: u64, key: K) -> u32;
}

/// Stops with an error of the shard's memory: past a bound, or memory the kernel refused.
#[cold]
#[track_caller]
fn stop(error: MemError) -> ! {
    panic!("{error}")
}

/// Where a probe ended: at the entry it looked for, or at the empty slot the entry would take.
enum Probe {
    Found(u32),
    Empty(usize),
}

/// A shard's hash index: slots of `(hash >> 32) << 32 | (entry + 1)`, 0 for empty, a power of
/// two of them, at most half full.
pub(crate) struct Index {
    slots: RegionVec<u64>,
}

impl Index {
    fn new() -> Result<Index, MemError> {
        Ok(Index { slots: RegionVec::new_huge(MAX_SLOTS, Tag::Link)? })
    }

    #[inline]
    fn slot(hash: u64, entry: u32) -> u64 {
        ((hash >> 32) << 32) | u64::from(entry + 1)
    }

    #[inline]
    fn probe(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Probe {
        let slots = self.slots.as_slice();
        if slots.is_empty() {
            return Probe::Empty(0);
        }
        let mask = slots.len() - 1;
        let tag = hash >> 32;
        let mut at = hash as usize & mask;
        loop {
            let slot = slots[at];
            if slot == 0 {
                return Probe::Empty(at);
            }
            if slot >> 32 == tag {
                let entry = slot as u32 - 1;
                if eq(entry) {
                    return Probe::Found(entry);
                }
            }
            at = (at + 1) & mask;
        }
    }

    #[inline]
    fn find(&self, hash: u64, eq: impl FnMut(u32) -> bool) -> Option<u32> {
        match self.probe(hash, eq) {
            Probe::Found(entry) => Some(entry),
            Probe::Empty(_) => None,
        }
    }

    /// Puts `entry` (the shard's newest: entries `0..entry` are in) at `at`, where its probe
    /// ended; doubles the index first when it would be more than half full, rehashing the
    /// entries below it with `hash_of`.
    #[inline]
    fn insert(&mut self, at: usize, hash: u64, entry: u32, hash_of: impl Fn(u32) -> u64) -> Result<(), MemError> {
        if (entry as usize + 1) * 2 > self.slots.len() {
            self.grow(entry, hash_of)?;
            self.put(hash, entry);
            return Ok(());
        }
        self.slots[at] = Index::slot(hash, entry);
        Ok(())
    }

    /// Doubles the slots in place (committing pages, never moving) and puts entries
    /// `0..entries` back: O(entries) once per doubling, as a SwissTable's rehash is, with no
    /// second table.
    #[cold]
    fn grow(&mut self, entries: u32, hash_of: impl Fn(u32) -> u64) -> Result<(), MemError> {
        let old = self.slots.len();
        let want = (old * 2).max(MIN_SLOTS);
        if want > MAX_SLOTS {
            return Err(MemError::Exhausted { tag: Tag::Link, reserved: MAX_SLOTS * 8, wanted: want * 8 });
        }
        self.slots.reserve(want - old)?;
        self.slots.extend(std::iter::repeat_n(0, want - old));
        self.slots[..old].fill(0);
        for entry in 0..entries {
            self.put(hash_of(entry), entry);
        }
        Ok(())
    }

    /// Puts `entry` in the first empty slot from its hash's: the index holds room for it.
    #[inline]
    fn put(&mut self, hash: u64, entry: u32) {
        let slots = self.slots.as_mut_slice();
        let mask = slots.len() - 1;
        let mut at = hash as usize & mask;
        while slots[at] != 0 {
            at = (at + 1) & mask;
        }
        slots[at] = Index::slot(hash, entry);
    }
}

/// Symbol keys: a [`SymKey`] per entry. A key's hash is three multiplies, so the index is
/// rebuilt from the keys rather than keeping 8 bytes of hash beside each.
pub(crate) struct KeyShard {
    index: Index,
    keys: RegionVec<SymKey>,
}

impl KeyShard {
    pub(crate) fn new() -> Result<KeyShard, MemError> {
        Ok(KeyShard { index: Index::new()?, keys: RegionVec::new(MAX_ENTRIES, Tag::Link)? })
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    #[inline]
    pub(crate) fn key(&self, at: usize) -> SymKey {
        self.keys[at]
    }

    #[inline]
    pub(crate) fn find(&self, hash: u64, key: SymKey) -> Option<u32> {
        let keys = &self.keys;
        self.index.find(hash, |at| keys[at as usize] == key)
    }
}

impl Shard<SymKey> for KeyShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, key: SymKey) -> u32 {
        let KeyShard { index, keys } = self;
        let found = {
            let keys = keys.as_slice();
            index.probe(hash, |at| keys[at as usize] == key)
        };
        let at = match found {
            Probe::Found(entry) => return entry,
            Probe::Empty(at) => at,
        };
        let entry = keys.try_push(key).unwrap_or_else(|error| stop(error)) as u32;
        let keys = keys.as_slice();
        if let Err(error) = index.insert(at, hash, entry, |at| hash_key(keys[at as usize])) {
            stop(error)
        }
        entry
    }
}

/// Names: every name of the shard in one byte region, each the bytes up to its end offset.
/// The index is rebuilt by hashing the names again: once per doubling, so at most twice per
/// name over the shard's life.
pub(crate) struct NameShard {
    index: Index,
    bytes: RegionVec<u8>,
    ends: RegionVec<u32>,
}

/// Name `at` of a shard: the bytes from the end of the one before it to its own end.
#[inline]
#[allow(unsafe_code)]
fn name_at<'s>(bytes: &'s [u8], ends: &[u32], at: u32) -> &'s str {
    let at = at as usize;
    let start = if at == 0 { 0 } else { ends[at - 1] as usize };
    let name = &bytes[start..ends[at] as usize];
    debug_assert!(std::str::from_utf8(name).is_ok());
    // SAFETY: `NameShard::get_or_insert` is the only writer of `bytes` and `ends`: it appends a
    // `&str`'s bytes and then pushes the new length as that name's end, so each range between
    // consecutive ends is exactly one appended `&str`, which is UTF-8. Nothing hands out
    // `&mut` to either column, and the slice above checked the range's bounds.
    unsafe { std::str::from_utf8_unchecked(name) }
}

impl NameShard {
    pub(crate) fn new() -> Result<NameShard, MemError> {
        Ok(NameShard {
            index: Index::new()?,
            bytes: RegionVec::new(MAX_NAME_BYTES, Tag::Link)?,
            ends: RegionVec::new(MAX_ENTRIES, Tag::Link)?,
        })
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    #[inline]
    pub(crate) fn get(&self, at: usize) -> &str {
        name_at(&self.bytes, &self.ends, at as u32)
    }

    #[inline]
    pub(crate) fn find(&self, hash: u64, name: &str) -> Option<u32> {
        let (bytes, ends) = (&self.bytes, &self.ends);
        self.index.find(hash, |at| name_at(bytes, ends, at) == name)
    }
}

impl<'k> Shard<&'k str> for NameShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, name: &'k str) -> u32 {
        let NameShard { index, bytes, ends } = self;
        let found = {
            let (bytes, ends) = (bytes.as_slice(), ends.as_slice());
            index.probe(hash, |at| name_at(bytes, ends, at) == name)
        };
        let at = match found {
            Probe::Found(entry) => return entry,
            Probe::Empty(at) => at,
        };
        if let Err(error) = bytes.try_extend_from_slice(name.as_bytes()) {
            stop(error)
        }
        let end = bytes.len() as u32;
        let entry = ends.try_push(end).unwrap_or_else(|error| stop(error)) as u32;
        let (bytes, ends) = (bytes.as_slice(), ends.as_slice());
        if let Err(error) = index.insert(at, hash, entry, |at| hash_name(name_at(bytes, ends, at))) {
            stop(error)
        }
        entry
    }
}

/// Content ids: a [`ContentId`] per entry, whose hash is its first eight bytes.
pub(crate) struct IdShard {
    index: Index,
    ids: RegionVec<ContentId>,
}

impl IdShard {
    pub(crate) fn new() -> Result<IdShard, MemError> {
        Ok(IdShard { index: Index::new()?, ids: RegionVec::new(MAX_ENTRIES, Tag::Link)? })
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    #[inline]
    pub(crate) fn id(&self, at: usize) -> &ContentId {
        &self.ids[at]
    }

    #[inline]
    pub(crate) fn find(&self, hash: u64, id: &ContentId) -> Option<u32> {
        let ids = &self.ids;
        self.index.find(hash, |at| ids[at as usize] == *id)
    }
}

impl<'k> Shard<&'k ContentId> for IdShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, id: &'k ContentId) -> u32 {
        let IdShard { index, ids } = self;
        let found = {
            let ids = ids.as_slice();
            index.probe(hash, |at| ids[at as usize] == *id)
        };
        let at = match found {
            Probe::Found(entry) => return entry,
            Probe::Empty(at) => at,
        };
        let entry = ids.try_push(*id).unwrap_or_else(|error| stop(error)) as u32;
        let ids = ids.as_slice();
        if let Err(error) = index.insert(at, hash, entry, |at| hash_id(&ids[at as usize])) {
            stop(error)
        }
        entry
    }
}

/// [`SHARDS`] shards, each made by `make`: once per image.
pub(crate) fn shards<S>(make: fn() -> Result<S, MemError>) -> Result<Vec<S>, MemError> {
    // alloc: once per image, the list of its 64 shards (each shard's tables are regions)
    let mut out = Vec::with_capacity(SHARDS);
    for _ in 0..SHARDS {
        out.push(make()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Relocs, Target};
    use blaze_core::{Completion, Done, Effect, FileName};

    use super::*;
    use crate::model::Sym;
    use crate::symbols::{Symbols, Wide};
    use crate::{Change, Debug, Format, Image, LinkError, Output, OutputKind};

    /// More entries than one commit step of every column (64 KiB: 3277 keys, 2048 ids, 8192
    /// index slots) several times over, and six doublings of the index.
    const GROWN: u32 = 40_000;

    /// One-function relinks enough to take every shard of every table of a 4098-function image
    /// past its first commit step: 150 000 relinks of 5 keys, 2 ids and a 38-byte name are, per
    /// shard, 234 KiB of keys, 150 KiB of ids and 89 KiB of names.
    const RELINKS: u32 = 150_000;

    fn content(n: u32) -> ContentId {
        ContentId::of("blaze v2 link intern test", &n.to_le_bytes())
    }

    fn key(n: u32) -> SymKey {
        SymKey::Local(crate::symbols::Obj { native: crate::symbols::IdNo(n % 7), member: n / 7 }, n)
    }

    fn name(n: u32) -> String {
        format!("_ZN5blaze4link{n}17h0123456789abcdefE")
    }

    /// The shard as the `Vec`-and-`HashMap` table it replaces: the same keys in the same order
    /// get the same indexes, and each index reads back the key it was given.
    #[test]
    fn shards_intern_as_a_map_and_a_vector_would() {
        let mut keys = KeyShard::new().unwrap();
        let mut names = NameShard::new().unwrap();
        let mut ids = IdShard::new().unwrap();
        let mut reference_keys: HashMap<SymKey, u32> = HashMap::new();
        let mut reference_names: HashMap<String, u32> = HashMap::new();
        let mut reference_ids: HashMap<ContentId, u32> = HashMap::new();
        let mut name_list: Vec<String> = Vec::new();
        for round in 0..2 {
            for n in 0..GROWN {
                // Each value twice, the repeats interleaved with new ones.
                let n = if round == 0 { n } else { (n * 7919) % GROWN };
                let (k, s, i) = (key(n), name(n), content(n));
                let next = reference_keys.len() as u32;
                let want = *reference_keys.entry(k).or_insert(next);
                assert_eq!(keys.get_or_insert(hash_key(k), k), want, "key {n}");
                let next = reference_names.len() as u32;
                let want = *reference_names.entry(s.clone()).or_insert_with(|| {
                    name_list.push(s.clone());
                    next
                });
                assert_eq!(names.get_or_insert(hash_name(&s), &s), want, "name {n}");
                let next = reference_ids.len() as u32;
                let want = *reference_ids.entry(i).or_insert(next);
                assert_eq!(ids.get_or_insert(hash_id(&i), &i), want, "id {n}");
            }
        }
        assert_eq!((keys.len(), names.len(), ids.len()), (GROWN as usize, GROWN as usize, GROWN as usize));
        for (k, at) in &reference_keys {
            assert_eq!(keys.key(*at as usize), *k);
            assert_eq!(keys.find(hash_key(*k), *k), Some(*at));
        }
        for (at, s) in name_list.iter().enumerate() {
            assert_eq!(names.get(at), s.as_str());
            assert_eq!(names.find(hash_name(s), s), Some(at as u32));
        }
        assert_eq!(names.bytes.as_slice(), name_list.concat().as_bytes(), "the names' bytes, in order");
        for (i, at) in &reference_ids {
            assert_eq!(ids.id(*at as usize), i);
            assert_eq!(ids.find(hash_id(i), i), Some(*at));
        }
        assert_eq!(keys.find(hash_key(key(GROWN + 1)), key(GROWN + 1)), None);
        assert_eq!(names.find(hash_name("absent"), "absent"), None);
        assert_eq!(ids.find(hash_id(&content(GROWN + 1)), &content(GROWN + 1)), None);
        assert!(keys.index.slots.len() >= 2 * GROWN as usize && keys.index.slots.len().is_power_of_two());
    }

    /// The `unsafe` view of a name against the checked one, for names of every UTF-8 width.
    #[test]
    fn names_read_back_as_their_checked_bytes() {
        let mut names = NameShard::new().unwrap();
        let all = ["", "a", "é", "日本", "🦀x", "memcpy", ""];
        for it in all {
            names.get_or_insert(hash_name(it), it);
        }
        for at in 0..names.len() {
            let start = if at == 0 { 0 } else { names.ends[at - 1] as usize };
            let checked = std::str::from_utf8(&names.bytes[start..names.ends[at] as usize]).unwrap();
            assert_eq!(names.get(at), checked);
        }
        assert_eq!(names.len(), all.len() - 1, "the empty name is interned once");
    }

    /// Growth past every commit step and index doubling makes no global allocation: the
    /// regions commit pages, the index rebuilds in place.
    #[test]
    fn table_growth_allocates_nothing() {
        let mut keys = KeyShard::new().unwrap();
        let mut names = NameShard::new().unwrap();
        let mut ids = IdShard::new().unwrap();
        let inputs: Vec<(SymKey, String, ContentId)> = (0..GROWN).map(|n| (key(n), name(n), content(n))).collect();
        blaze_alloc_count::assert_at_most(0, "interning 40 000 new keys, names and ids into one shard each", || {
            for (k, s, i) in &inputs {
                keys.get_or_insert(hash_key(*k), *k);
                names.get_or_insert(hash_name(s), s);
                ids.get_or_insert(hash_id(i), i);
            }
        });
        blaze_alloc_count::assert_at_most(0, "lookups", || {
            for (k, s, i) in &inputs {
                assert!(keys.find(hash_key(*k), *k).is_some());
                assert!(names.find(hash_name(s), s).is_some());
                assert!(ids.find(hash_id(i), i).is_some());
            }
        });
    }

    fn output() -> Output {
        Output {
            format: Format::Elf,
            arch: Arch::X86_64,
            kind: OutputKind::Executable { entry: "_start".into() },
            interpreter: Some("/lib64/ld-linux-x86-64.so.2".into()),
            platform: None,
            incremental: true,
            debug: Debug::None,
            live: None,
            tags: 1 << 32,
            windows_subsystem: false,
            dev_run: None,
            exec_log: false,
            sanitize: blaze_codegen::settings::Sanitize::default(),
            rpaths: Vec::new(),
            triple: "x86_64-unknown-linux-gnu".into(),
            stack: None,
        }
    }

    fn instance(n: u32) -> InstanceKey {
        let mut bytes = [0x5a; 32];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        InstanceKey(ContentId(bytes))
    }

    /// Function `n`: `mov eax, 1; ret`.
    fn function(n: u32) -> CompiledFunction {
        CompiledFunction {
            key: instance(n),
            arch: Arch::X86_64,
            align: 16,
            code: vec![0xb8, 1, 0, 0, 0, 0xc3],
            relocs: Relocs::new(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    /// Stages and commits `change`, completing every effect as a driver whose disk never fails.
    fn link(image: &mut Image, change: Change<'_>) -> Result<u64, LinkError> {
        let staged = image.stage(change)?;
        let mut effects = Vec::new();
        image.commit(staged, &mut effects)?;
        loop {
            let effect = effects.pop().expect("the commit ended or asked for more");
            let done = match &effect {
                Effect::Remove { .. } => Done::Removed,
                Effect::Truncate { .. } => Done::Truncated,
                Effect::Clone { .. } => Done::Cloned,
                Effect::WriteAt { .. } | Effect::Write { .. } => Done::Written,
                Effect::Sync { .. } => Done::Synced,
                Effect::Mode { .. } => Done::Moded,
                Effect::Rename { .. } => Done::Renamed,
                other => panic!("the linker asked for {other:?}"),
            };
            let mut more = Vec::new();
            if let Some(result) = image.complete(&Completion { tag: effect.tag(), done }, &mut more) {
                return result;
            }
            effects.extend(more);
        }
    }

    /// An image of 4098 functions linked, then relinked after a one-function change: the next
    /// relinks' table work (the changed function's body, slot, data and names interned, each
    /// new) grows the warm image's symbol table without one global allocation, through as many
    /// relinks as take every shard past a commit step.
    #[test]
    fn a_warm_relinks_table_growth_allocates_nothing() {
        let all: Vec<CompiledFunction> = (0..4098).map(function).collect();
        let mut image = Image::new(FileName::new("out/intern").unwrap(), output()).unwrap();
        let first = Change {
            added: all.iter().collect(),
            names: vec![("_start".into(), Target::Direct(instance(0)))],
            ..Change::default()
        };
        link(&mut image, first).unwrap();
        let edited = function(1_000_000);
        let edit = Change {
            added: vec![&edited],
            removed: vec![instance(1)],
            slots: vec![(instance(1), instance(1_000_000))],
            ..Change::default()
        };
        link(&mut image, edit).unwrap();
        let symbols = &mut image.symbols;
        let changes: Vec<(InstanceKey, ContentId, String)> =
            (0..RELINKS).map(|n| (instance(2_000_000 + n), content(n), name(n))).collect();
        let before = symbols.dense_len();
        let last: Sym = blaze_alloc_count::assert_at_most(0, "a warm relink's symbol table growth", || {
            let mut last = Sym(0);
            for (function, datum, extern_name) in &changes {
                symbols.body(function);
                let id = symbols.id(&function.0);
                symbols.intern(SymKey::Slot(id));
                let data = symbols.id(datum);
                symbols.intern(SymKey::Data(data));
                symbols.sym_name(extern_name);
                last = symbols.intern(SymKey::Position(id, 3));
            }
            last
        });
        assert!(symbols.dense_len() > before, "the relinks grew the table");
        let (function, datum, extern_name) = &changes[RELINKS as usize - 1];
        let id = symbols.find_id(&function.0).expect("interned");
        assert_eq!(symbols.find(SymKey::Position(id, 3)), Some(last));
        assert_eq!(symbols.find_body(function), Some(symbols.body(function)));
        assert!(symbols.find_slot(function).is_some());
        assert!(symbols.find_id(datum).is_some());
        assert_eq!(symbols.name_of(symbols.find_sym_name(extern_name).unwrap()), Some(extern_name.as_str()));
        let batch: Vec<Wide<'_>> = changes.iter().map(|it| Wide::Body(&it.0)).collect();
        let again = symbols.intern_wide(&batch);
        assert_eq!(again[7], symbols.body(&changes[7].0), "a batch finds what the warm path interned");
    }

    /// A new image's symbol table reserves its regions and commits nothing: making one costs
    /// the three lists of shards, whatever the bounds.
    #[test]
    fn a_new_table_reserves_and_commits_nothing() {
        assert!(RESERVED < 11 << 30, "{RESERVED} bytes of address space per image");
        let (symbols, used) = blaze_alloc_count::measure(|| Symbols::new().unwrap());
        assert_eq!(used.allocations, 3, "the lists of key, name and id shards");
        assert!(symbols.find_sym_name("main").is_none());
        assert_eq!(symbols.dense_len(), 0);
    }
}
