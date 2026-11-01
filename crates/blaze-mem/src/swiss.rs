//! The SwissTable group probe every interner's table shares (docs/v2/plan/design/hardware.md,
//! "interner and memo lookups"): `intern`'s arena store and mapped index, and the query engine's
//! interned key map. `intern` re-exports it as `intern::swiss`; it lives here, a
//! leaf every table's crate already depends on, so `blaze-query` and `blaze-ids` reach it without
//! `intern`'s image and format crates.
//!
//! A table is a power of two of groups; a group is a [`Ctrl`] word of tag bytes beside its
//! slots. A lookup of `hash` starts at the group `hash & mask` and goes on group by group
//! until one has an empty slot, comparing only the slots whose tag is the hash's top seven
//! bits. A group is one word, so matching its slots is a few ALU operations and no branch per
//! slot (SWAR, as hashbrown's generic group): a 64-bit word holds eight tags, where SSE2 or
//! NEON would hold sixteen, but a slot of a pointer or ID stays in the group's own line, which
//! a 16-wide group can't.
//!
//! [`find`] and [`vacant`] load no memory: the caller hands them a closure that loads a
//! group's word, with whatever atomics and ordering its table uses (std's, the model
//! checker's, shuttle's, or a mapped `&[u64]`), so one probe serves them all. [`Index`] is the
//! one table built on them here: `u32`s naming a column the caller keeps, which every
//! interner keyed by an index (`blaze_ids::Sharded`, the hash-conser, base-db's tables) uses.

use crate::sync::{
    OnceLock,
    atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

/// The tag of an empty slot.
pub const EMPTY: u8 = 0;

/// The tag of a slot whose key was removed: full, so probes go on past it, and never a key's
/// tag in a table that removes ([`live_tag`]).
pub const DEAD: u8 = 0x80;

/// A full slot's tag: `0x80 |` the hash's top seven bits. Tables that never remove a key use
/// it; it is part of `intern`'s image layout, so it never changes.
#[inline]
pub fn tag(hash: u64) -> u8 {
    0x80 | (hash >> 57) as u8
}

/// [`tag`] with 0 folded into 1, so no key's tag is [`DEAD`]: for tables that remove keys.
#[inline]
pub fn live_tag(hash: u64) -> u8 {
    0x80 | ((hash >> 57) as u8).max(1)
}

/// A group's control word: byte `i` (from the least significant) is slot `i`'s tag, [`EMPTY`]
/// or `0x80 |` seven bits. Bytes `SLOTS..8` stay 0 and are masked out of every answer, so a
/// group of seven pointers and its word fill one 64-byte line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ctrl<const SLOTS: usize>(pub u64);

impl<const SLOTS: usize> Ctrl<SLOTS> {
    /// Bit 0 of each slot byte.
    pub const LOW: u64 = {
        assert!(SLOTS >= 1 && SLOTS <= 8, "a control word holds one to eight slots");
        let mut low = 0u64;
        let mut slot = 0;
        while slot < SLOTS {
            low |= 1 << (slot * 8);
            slot += 1;
        }
        low
    };
    /// Bit 7 of each slot byte.
    pub const HIGH: u64 = Self::LOW << 7;
    /// Bits 0 to 6 of each slot byte.
    const SEVEN: u64 = Self::LOW * 0x7f;

    /// The slots tagged `tag`, exactly: `x` is zero in the bytes equal to `tag`, and a byte's
    /// low seven bits plus `0x7f` sets its top bit exactly when they are not all zero, without
    /// carrying into the next byte.
    #[inline]
    pub fn matching(self, tag: u8) -> Slots {
        let x = self.0 ^ (Self::LOW * u64::from(tag));
        Slots(!(((x & Self::SEVEN) + Self::SEVEN) | x) & Self::HIGH)
    }

    /// The empty slots: a full byte has its top bit set.
    #[inline]
    pub fn empty(self) -> Slots {
        Slots(!self.0 & Self::HIGH)
    }

    /// The full slots, dead ones included.
    #[inline]
    pub fn full(self) -> Slots {
        Slots(self.0 & Self::HIGH)
    }

    /// The full slots that hold a key: not [`DEAD`], whose low seven bits are all zero.
    #[inline]
    pub fn live(self) -> Slots {
        Slots(self.0 & ((self.0 & Self::SEVEN) + Self::SEVEN) & Self::HIGH)
    }

    /// Slot `slot`'s tag.
    #[inline]
    pub fn tag_at(self, slot: usize) -> u8 {
        debug_assert!(slot < SLOTS);
        (self.0 >> (slot * 8)) as u8
    }

    /// The word with slot `slot`'s tag set to `tag`.
    #[inline]
    pub fn with(self, slot: usize, tag: u8) -> Ctrl<SLOTS> {
        debug_assert!(slot < SLOTS);
        let shift = slot * 8;
        Ctrl((self.0 & !(0xff << shift)) | (u64::from(tag) << shift))
    }
}

/// A set of a group's slots, as the top bit of each one's byte; iterates in slot order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slots(u64);

impl Slots {
    #[inline]
    pub fn any(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub fn first(self) -> Option<usize> {
        self.any().then(|| (self.0.trailing_zeros() / 8) as usize)
    }
}

impl Iterator for Slots {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        let slot = self.first()?;
        self.0 &= self.0 - 1;
        Some(slot)
    }
}

/// A slot of a table: its group, its place in the group, and the group's word as the probe
/// loaded it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct At<const SLOTS: usize> {
    pub group: usize,
    pub slot: usize,
    pub ctrl: Ctrl<SLOTS>,
}

impl<const SLOTS: usize> At<SLOTS> {
    /// The slot's index among the table's, for a column beside the groups (a slot's hash).
    #[inline]
    pub fn index(self) -> usize {
        self.group * SLOTS + self.slot
    }

    /// The group's word with this slot tagged `tag`: what the inserter publishes.
    #[inline]
    pub fn tagged(self, tag: u8) -> Ctrl<SLOTS> {
        self.ctrl.with(self.slot, tag)
    }
}

/// The first slot tagged `tag` that `hit` accepts, probing from `hash`'s home group in a table
/// of `groups` (a power of two) until a group with an empty slot. `ctrl` loads group `g`'s
/// word; a table never fills, so a miss ends.
#[inline]
pub fn find<const SLOTS: usize, T>(
    hash: u64,
    tag: u8,
    groups: usize,
    mut ctrl: impl FnMut(usize) -> Ctrl<SLOTS>,
    mut hit: impl FnMut(At<SLOTS>) -> Option<T>,
) -> Option<T> {
    debug_assert!(groups.is_power_of_two());
    let mask = groups - 1;
    let mut group = hash as usize & mask;
    loop {
        let word = ctrl(group);
        for slot in word.matching(tag) {
            if let Some(found) = hit(At { group, slot, ctrl: word }) {
                return Some(found);
            }
        }
        if word.empty().any() {
            return None;
        }
        group = (group + 1) & mask;
    }
}

/// The first empty slot from `hash`'s home group, where an insert of `hash` goes: the group a
/// [`find`] of it stops at. For the table's one writer.
#[inline]
pub fn vacant<const SLOTS: usize>(
    hash: u64,
    groups: usize,
    mut ctrl: impl FnMut(usize) -> Ctrl<SLOTS>,
) -> At<SLOTS> {
    debug_assert!(groups.is_power_of_two());
    let mask = groups - 1;
    let mut group = hash as usize & mask;
    loop {
        let word = ctrl(group);
        if let Some(slot) = word.empty().first() {
            return At { group, slot, ctrl: word };
        }
        group = (group + 1) & mask;
    }
}

/// Slots in a group of an [`Index`]: a control word and six `u32`s, 32 bytes aligned to 32, so
/// two groups fill a 64-byte line and none straddles two. Eight slots made a 40-byte group, half
/// of which crossed a line: about a third of the slots a hit reads were a line away from their
/// control word.
const INDEX_SLOTS: usize = 6;
/// Table `k` of an [`Index`] has `1 << k` groups: 31 levels hold more than `2^32` values at
/// three quarters full (`2^30 × 6 × 3 / 4`).
const LEVELS: usize = 31;

#[repr(C, align(32))]
struct Group {
    ctrl: AtomicU64,
    values: [AtomicU32; INDEX_SLOTS],
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Group>() == 32 && align_of::<Group>() == 32);

fn new_table(len: usize) -> Box<[Group]> {
    (0..len)
        .map(|_| Group { ctrl: AtomicU64::new(0), values: std::array::from_fn(|_| AtomicU32::new(0)) })
        // alloc: growth, one table per doubling of the values, over the inserts that filled the last
        .collect()
}

/// A SwissTable of `u32`s, each naming a value in a column the caller keeps, found by a hash
/// the caller computed once: what an interner keyed by a dense index puts beside its arena.
///
/// A [`Index::find`] takes no lock and writes nothing: an acquire load of the level, one of
/// the home group's word (almost always the only one) and one slot from the same 32 bytes, one
/// cache line, then the caller's check, which reads its column only on a tag match (one in 128
/// for another value). Inserts are the caller's to serialize (its shard's lock). A table fills to three
/// quarters, then the next level, twice its size, is filled from it and published with one
/// release store; the levels it outgrew stay readable, as a reader may be probing one, until
/// [`Index::release_outgrown`] frees them with `&mut`. A value costs 7.1 to 14.2 bytes of
/// table, and nothing is stored of its hash: growing asks the caller for it.
///
/// An index with no value has no table: [`Index::new`] allocates nothing.
pub struct Index {
    /// How many levels are published; the last is in use.
    level: AtomicUsize,
    tables: [OnceLock<Box<[Group]>>; LEVELS],
}

impl Default for Index {
    fn default() -> Index {
        Index::new()
    }
}

impl Index {
    pub fn new() -> Index {
        Index { level: AtomicUsize::new(0), tables: std::array::from_fn(|_| OnceLock::new()) }
    }

    fn at_level(&self, level: usize) -> Option<&[Group]> {
        let level = level.checked_sub(1)?;
        Some(self.tables[level].get().expect("a published index level holds its table"))
    }

    /// The value inserted under `hash` that `is` accepts. `is` is asked only of values whose
    /// insert was published, so what the caller wrote of one before [`Index::insert`] is there.
    #[inline]
    pub fn find(&self, hash: u64, mut is: impl FnMut(u32) -> bool) -> Option<u32> {
        let groups = self.at_level(self.level.load(Ordering::Acquire))?;
        let ctrl = |at: usize| Ctrl::<INDEX_SLOTS>(groups[at].ctrl.load(Ordering::Acquire));
        find(hash, tag(hash), groups.len(), ctrl, |at| {
            // Relaxed: the tag's `Acquire` ordered the slot's store before us.
            let value = groups[at.group].values[at.slot].load(Ordering::Relaxed);
            is(value).then_some(value)
        })
    }

    /// Publishes `value` under `hash`, for the one writer at a time the caller lets in. `len` is
    /// how many values the index held before this one, and `hash_of` the hash a held value was
    /// inserted under, which growing asks for. Whatever `find`'s `is` reads of `value` must be
    /// written before this call.
    pub fn insert(&self, len: usize, hash: u64, value: u32, hash_of: impl Fn(u32) -> u64) {
        let groups = self.table_for_one_more(len, hash_of);
        put(groups, hash, value, Ordering::Release);
    }

    /// The table to insert one more value into, at most three quarters full after it.
    fn table_for_one_more(&self, len: usize, hash_of: impl Fn(u32) -> u64) -> &[Group] {
        // Relaxed: only the one writer stores `level`.
        let level = self.level.load(Ordering::Relaxed);
        let current = self.at_level(level);
        if let Some(groups) = current
            && (len + 1) * 4 <= groups.len() * INDEX_SLOTS * 3
        {
            return groups;
        }
        assert!(level < LEVELS, "an index of more than 2^32 values");
        let grown = new_table(1 << level);
        if let Some(old) = current {
            // Relaxed: every write to the table was the writer's, and so ours.
            for group in old {
                for slot in Ctrl::<INDEX_SLOTS>(group.ctrl.load(Ordering::Relaxed)).full() {
                    let value = group.values[slot].load(Ordering::Relaxed);
                    put(&grown, hash_of(value), value, Ordering::Relaxed);
                }
            }
        }
        if self.tables[level].set(grown).is_err() {
            unreachable!("index level {level} published twice");
        }
        // Release: a reader that loads the new level sees the table's slots and, through the
        // caller's order, the values they name.
        self.level.store(level + 1, Ordering::Release);
        self.at_level(level + 1).expect("the level just published")
    }

    /// How many values the table in use holds at most before it grows: 0 before the first insert.
    pub fn capacity(&self) -> usize {
        self.at_level(self.level.load(Ordering::Acquire)).map_or(0, |groups| groups.len() * INDEX_SLOTS * 3 / 4)
    }

    /// Frees the tables the index outgrew. With `&mut`, no reader is probing one.
    pub fn release_outgrown(&mut self) {
        let level = self.level.load(Ordering::Relaxed);
        for table in &mut self.tables[..level.saturating_sub(1)] {
            *table = OnceLock::new();
        }
    }
}

/// Fills the first empty slot from `hash`'s home group of `groups`, for the one writer.
fn put(groups: &[Group], hash: u64, value: u32, publish: Ordering) {
    let at = vacant(hash, groups.len(), |at: usize| Ctrl::<INDEX_SLOTS>(groups[at].ctrl.load(Ordering::Relaxed)));
    let group = &groups[at.group];
    group.values[at.slot].store(value, Ordering::Relaxed);
    group.ctrl.store(at.tagged(tag(hash)).0, publish);
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::*;

    /// The copies this module replaced (`intern`'s arena and mapped index, the old query
    /// engine's interned table and node map), as the spec: the subtraction trick, whose borrow
    /// may also flag the byte above a match, filtered by comparing the byte.
    mod reference {
        pub fn matching(word: u64, tag: u64, low: u64) -> u64 {
            let high = low << 7;
            let x = word ^ (low * tag);
            let mut hits = x.wrapping_sub(low) & !x & high;
            let mut exact = 0;
            while hits != 0 {
                let byte = hits.trailing_zeros() / 8;
                hits &= hits - 1;
                if (word >> (byte * 8)) & 0xff == tag {
                    exact |= 0x80 << (byte * 8);
                }
            }
            exact
        }

        pub fn empty(word: u64, low: u64) -> u64 {
            !word & (low << 7)
        }

        pub fn live(word: u64, low: u64) -> u64 {
            let high = low << 7;
            word & high & ((word & !high).wrapping_add(low * 0x7f) & high)
        }

        /// The probe loop as each copy wrote it, over plain words.
        pub fn find(words: &[u64], slots: usize, hash: u64, tag: u64, low: u64, hit: impl Fn(usize) -> bool) -> Option<usize> {
            let mask = words.len() - 1;
            let mut at = hash as usize & mask;
            loop {
                let word = words[at];
                let mut hits = matching(word, tag, low);
                while hits != 0 {
                    let byte = (hits.trailing_zeros() / 8) as usize;
                    hits &= hits - 1;
                    if hit(at * slots + byte) {
                        return Some(at * slots + byte);
                    }
                }
                if empty(word, low) != 0 {
                    return None;
                }
                at = (at + 1) & mask;
            }
        }
    }

    fn bits(slots: Slots) -> u64 {
        slots.fold(0, |bits, slot| bits | 0x80 << (slot * 8))
    }

    /// A word of `SLOTS` slot bytes drawn from `bytes`, empty, dead or tags, the rest zero.
    fn word<const SLOTS: usize>(bytes: [u8; 8]) -> Ctrl<SLOTS> {
        let mut word = 0;
        for (slot, &byte) in bytes.iter().enumerate().take(SLOTS) {
            let byte = match byte % 4 {
                0 => EMPTY,
                1 => DEAD,
                _ => 0x80 | byte,
            };
            word |= u64::from(byte) << (slot * 8);
        }
        Ctrl(word)
    }

    fn agrees<const SLOTS: usize>(bytes: [u8; 8], tag: u8) {
        let word = word::<SLOTS>(bytes);
        let low = Ctrl::<SLOTS>::LOW;
        assert_eq!(bits(word.matching(tag)), reference::matching(word.0, u64::from(tag), low), "{word:x?} {tag:x}");
        assert_eq!(bits(word.empty()), reference::empty(word.0, low));
        assert_eq!(bits(word.live()), reference::live(word.0, low));
        let naive: Vec<usize> = (0..SLOTS).filter(|&slot| word.tag_at(slot) == tag).collect();
        assert_eq!(word.matching(tag).collect::<Vec<_>>(), naive);
        let full: Vec<usize> = (0..SLOTS).filter(|&slot| word.tag_at(slot) & 0x80 != 0).collect();
        assert_eq!(word.full().collect::<Vec<_>>(), full);
    }

    #[hegel::test]
    fn words_agree_with_the_copies(tc: TestCase) {
        let bytes: [u8; 8] = tc.draw(gs::arrays(gs::integers()));
        let tag: u8 = tc.draw(gs::integers().min_value(0x80));
        agrees::<7>(bytes, tag);
        agrees::<8>(bytes, tag);
        agrees::<4>(bytes, tag);
    }

    /// `find` against the copies' loop, over tables a third to seven eighths full whose
    /// keys share few hashes, and `vacant` against the group `find` stops at.
    #[hegel::test]
    fn probes_agree_with_the_copies(tc: TestCase) {
        let seed: u64 = tc.draw(gs::integers());
        let log_groups: u32 = tc.draw(gs::integers().max_value(5));
        let fill: u64 = tc.draw(gs::integers().min_value(3).max_value(7));
        let hash: u64 = tc.draw(gs::integers());
        const SLOTS: usize = 7;
        let groups = 1usize << log_groups;
        let mut next = seed;
        let mut words = vec![0u64; groups];
        let mut keys = vec![u64::MAX; groups * SLOTS];
        let total = (groups * SLOTS) as u64 * fill / 8;
        for _ in 0..total {
            next = next.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let key = next >> 60;
            let key_hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (key << 57);
            let at = vacant::<SLOTS>(key_hash, groups, |g| Ctrl(words[g]));
            words[at.group] = at.tagged(tag(key_hash)).0;
            keys[at.index()] = key;
        }
        let key = hash >> 60;
        let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (key << 57);
        let low = Ctrl::<SLOTS>::LOW;
        let want = reference::find(&words, SLOTS, hash, u64::from(tag(hash)), low, |i| keys[i] == key);
        let got = find::<SLOTS, usize>(hash, tag(hash), groups, |g| Ctrl(words[g]), |at| (keys[at.index()] == key).then_some(at.index()));
        assert_eq!(got, want);
        if words.iter().any(|&w| Ctrl::<SLOTS>(w).empty().any()) {
            let stop = vacant::<SLOTS>(hash, groups, |g| Ctrl(words[g]));
            assert!(Ctrl::<SLOTS>(words[stop.group]).empty().any());
            assert_eq!(stop.ctrl.0, words[stop.group]);
        }
    }

    #[test]
    fn tags_are_full_and_live_tags_never_dead() {
        for top in 0..128u64 {
            assert_eq!(tag(top << 57), 0x80 | top as u8);
            assert!((0x81..=0xff).contains(&live_tag(top << 57)));
        }
    }

    /// The cases the copies tested, kept: every equal byte found among unequal and dead ones,
    /// the bytes past the slots never free, dead not free, and dead not live.
    #[test]
    fn matching_finds_every_equal_byte() {
        for tag in [0x80u8, 0x81, 0xfe, 0xff] {
            for slots in 0..=127u64 {
                let mut word = 0;
                let mut want = 0;
                for byte in 0..7 {
                    if slots & (1 << byte) != 0 {
                        word |= u64::from(tag) << (byte * 8);
                        want |= 0x80 << (byte * 8);
                    } else if byte % 3 == 0 {
                        word |= u64::from(tag ^ 1) << (byte * 8);
                    } else if byte % 3 == 1 && tag != DEAD {
                        word |= u64::from(DEAD) << (byte * 8);
                    }
                }
                assert_eq!(bits(Ctrl::<7>(word).matching(tag)), want, "{word:x} {tag:x}");
            }
        }
        assert_eq!(bits(Ctrl::<7>(0x0000_ff00_0081_0000).empty()), 0x0080_0080_8000_8080);
        assert_eq!(bits(Ctrl::<7>(0x00ff_ffff_ffff_ffff).empty()), 0, "byte 7 is never a free slot");
        assert_eq!(bits(Ctrl::<7>(0x80).empty()), 0x0080_8080_8080_8000, "dead is not free");
        assert_eq!(bits(Ctrl::<8>(0x0081_80ff_0000_8082).live()), 0x0080_0080_0000_0080);
        assert_eq!(bits(Ctrl::<8>(0x8080_8080_8080_8080).live()), 0);
    }

    #[test]
    fn live_skips_empty_and_dead() {
        for word in [0u64, 0x80, 0x0081_80ff_0000_8082, 0x00ff_ffff_ffff_ffff, 0x0080_8080_8080_8080] {
            let want: Vec<usize> = (0..7)
                .filter(|&slot| {
                    let byte = (word >> (slot * 8)) as u8;
                    byte & 0x80 != 0 && byte != DEAD
                })
                .collect();
            assert_eq!(Ctrl::<7>(word).live().collect::<Vec<_>>(), want, "{word:x}");
        }
    }

    #[test]
    fn with_sets_one_slot() {
        let word = Ctrl::<7>(0x0000_0000_0081_0000);
        assert_eq!(word.with(0, 0xaa).0, 0x0000_0000_0081_00aa);
        assert_eq!(word.with(2, DEAD).0, 0x0000_0000_0080_0000);
        assert_eq!(word.with(2, DEAD).tag_at(2), DEAD);
    }

    /// Few distinct hashes for many keys, some differing only in the tag or only in the home
    /// group, so tags and groups collide.
    fn hash_of(key: u64) -> u64 {
        match key % 3 {
            0 => key % 97,
            1 => ((key % 61) << 57) | 5,
            _ => key.wrapping_mul(0xff51_afd7_ed55_8ccd),
        }
    }

    /// The index against a `BTreeMap` numbering keys in first-seen order, over seeded keys:
    /// the same values found, every one kept across growths and `release_outgrown`.
    #[cfg(not(blaze_check))]
    #[test]
    fn index_agrees_with_a_map() {
        for seed in 0..4u64 {
            let mut index = Index::new();
            assert_eq!(index.capacity(), 0);
            let mut keys: Vec<u64> = Vec::new();
            let mut model: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
            let mut next = seed;
            for step in 0..40_000 {
                next = next.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let key = (next >> 33) % 9_000;
                let found = index.find(hash_of(key), |value| keys[value as usize] == key);
                assert_eq!(found, model.get(&key).copied(), "seed {seed} step {step}");
                if found.is_none() {
                    let value = keys.len() as u32;
                    keys.push(key);
                    index.insert(model.len(), hash_of(key), value, |value| hash_of(keys[value as usize]));
                    model.insert(key, value);
                }
                if step % 10_000 == 0 {
                    index.release_outgrown();
                }
            }
            assert!(index.capacity() >= model.len());
            index.release_outgrown();
            for (&key, &value) in &model {
                assert_eq!(index.find(hash_of(key), |it| keys[it as usize] == key), Some(value));
            }
        }
    }

    /// One writer at a time behind a lock, readers with none: every value a reader finds is
    /// the key's, and every key inserted before a reader starts is found.
    #[cfg(not(blaze_check))]
    #[test]
    #[expect(
        clippy::disallowed_types,
        reason = "the test's one writer at a time, which `Index` asks of its caller: a test thread, not the allocator"
    )]
    fn readers_race_a_growing_index() {
        use std::sync::atomic::AtomicU64 as Key;

        use parking_lot::Mutex;

        const KEYS: usize = 20_000;
        let index = Index::new();
        let column: Box<[Key]> = (0..KEYS).map(|_| Key::new(u64::MAX)).collect();
        let len = Mutex::new(0usize);
        let intern = |key: u64| -> u32 {
            let is = |value: u32| column[value as usize].load(Ordering::Relaxed) == key;
            if let Some(value) = index.find(hash_of(key), is) {
                return value;
            }
            let mut len = len.lock();
            if let Some(value) = index.find(hash_of(key), is) {
                return value;
            }
            let value = *len as u32;
            column[value as usize].store(key, Ordering::Relaxed);
            index.insert(*len, hash_of(key), value, |value| hash_of(column[value as usize].load(Ordering::Relaxed)));
            *len += 1;
            value
        };
        let early: Vec<u32> = (0..100).map(intern).collect();
        std::thread::scope(|scope| {
            for worker in 0..8u64 {
                let (intern, early, column, index) = (&intern, &early, &column, &index);
                scope.spawn(move || {
                    for n in 0..KEYS as u64 / 2 {
                        let key = (n * 7 + worker * 131) % (KEYS as u64 / 2);
                        let value = intern(key);
                        assert_eq!(column[value as usize].load(Ordering::Relaxed), key);
                        let first = (n % 100) as usize;
                        let found = index.find(hash_of(first as u64), |it| column[it as usize].load(Ordering::Relaxed) == first as u64);
                        assert_eq!(found, Some(early[first]));
                    }
                });
            }
        });
        assert_eq!(*len.lock(), KEYS / 2);
    }

    /// A probe reads one line: every group of every level starts on a 32-byte boundary, so its
    /// word and slots never cross a 64-byte line.
    #[cfg(not(blaze_check))]
    #[test]
    fn index_groups_never_straddle_a_line() {
        let index = Index::new();
        let keys: Vec<u64> = (0..5_000).collect();
        for (len, &key) in keys.iter().enumerate() {
            index.insert(len, hash_of(key), len as u32, |value| hash_of(keys[value as usize]));
        }
        let levels = index.level.load(Ordering::Relaxed);
        assert!(levels > 5);
        for table in index.tables[..levels].iter().filter_map(OnceLock::get) {
            for group in table.iter() {
                let at = std::ptr::from_ref(group).addr();
                assert_eq!(at / 64, (at + size_of::<Group>() - 1) / 64, "a group in one line");
            }
        }
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn an_index_allocates_only_to_grow() {
        let index = blaze_alloc_count::assert_at_most(0, "a new index", Index::new);
        let keys: Vec<u64> = (0..1_000).collect();
        for (len, &key) in keys.iter().enumerate() {
            index.insert(len, hash_of(key), len as u32, |value| hash_of(keys[value as usize]));
        }
        blaze_alloc_count::assert_at_most(0, "an index's finds", || {
            for &key in &keys {
                assert_eq!(index.find(hash_of(key), |it| keys[it as usize] == key), Some(key as u32));
            }
            assert_eq!(index.find(hash_of(5_000), |it| keys[it as usize] == 5_000), None);
        });
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn a_probe_allocates_nothing() {
        let words = [0x0081_8283_8485_8687u64, 0x0000_0000_0000_0081];
        blaze_alloc_count::assert_at_most(0, "a probe", || {
            let found = find::<7, usize>(1 << 57, 0x81, 2, |g| Ctrl(words[g]), |at| Some(at.index()));
            let free = vacant::<7>(0, 2, |g| Ctrl(words[g]));
            (found, free)
        });
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-mem --release
/// model_swiss`): one writer filling an [`Index`]'s first table and growing it while a reader
/// finds a value from before the growth and one inserted during it.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::Arc;

    use blaze_check::thread;

    use super::*;

    /// Every value is its own key, all of one hash: one home group and one tag, so every probe
    /// asks the caller and four values fill the first table (one group of six) to three
    /// quarters.
    const HASH: u64 = 3;

    #[test]
    fn model_swiss_reader_races_growth() {
        blaze_check::model(|| {
            let index = Arc::new(Index::new());
            for value in 0..4u32 {
                index.insert(value as usize, HASH, value, |_| HASH);
            }
            assert_eq!(index.capacity(), 4);
            let reader = {
                let index = index.clone();
                thread::spawn(move || (index.find(HASH, |it| it == 0), index.find(HASH, |it| it == 4)))
            };
            index.insert(4, HASH, 4, |_| HASH);
            index.insert(5, HASH, 5, |_| HASH);
            let (zero, four) = reader.join().unwrap();
            assert_eq!(zero, Some(0), "a value from before the growth is found");
            assert!(four.is_none() || four == Some(4), "a value is found whole or not at all");
            for value in 0..6 {
                assert_eq!(index.find(HASH, |it| it == value), Some(value), "kept across the growth");
            }
        });
    }
}
