//! The index table (docs/v2-store.md, "On disk"): an open-addressed table on keys, mapped and
//! read in place. A slot's number is the record's place in the store's flag columns, so it is
//! part of what `get` and `iter` give.
//!
//! The layout is SwissTable's: a [`INDEX_HEADER`]-byte header, a control column of one byte a
//! slot ([`EMPTY`] or a 7-bit tag of the key), then one [`ROW`]-byte row a slot holding the key
//! and its place. Slots come in groups of [`GROUP`]; a probe matches a group's control bytes
//! against the tag at once and reads a row only for a matching lane, so a hit is one cache line
//! of rows.

use blaze_ids::radix;

use crate::{
    Key, Kind,
    disk::{Form, Loc, id_at, u32_at, u64_at},
};

const INDEX_MAGIC: [u8; 8] = *b"BLZINDX\x03";
pub(crate) const INDEX_HEADER: usize = 64;
/// The bytes of one slot's row: the key, then its place.
const ROW: usize = 64;
/// Slots matched at once.
const GROUP: usize = 16;
/// The control byte of an empty slot; a full one holds its key's tag, below this.
const EMPTY: u8 = 0x80;
/// At most this share of an index's slots are full, so most probes end in their first group.
const LOAD_NUMERATOR: u64 = 7;
const LOAD_DENOMINATOR: u64 = 8;
const LEAST_SLOTS: u64 = 1024;

const _: () = assert!(INDEX_HEADER == 64);
const _: () = assert!(ROW == 64);
const _: () = assert!(size_of::<Key>() == 32);
// The rows start 64-byte aligned only while the control column is a multiple of 64 bytes.
const _: () = assert!(LEAST_SLOTS.is_power_of_two() && LEAST_SLOTS % 64 == 0);
const _: () = assert!(LEAST_SLOTS % GROUP as u64 == 0);

/// The lanes of a group whose control byte is `byte`: bit `i` for `group[i]`.
pub(crate) type Matcher = fn(&[u8; GROUP], u8) -> u16;

/// An index table of `entries`, whose bytes depend only on the set of entries. No key may
/// repeat.
///
/// Entries are placed in the order of their first group, then of their key: each probe starts
/// at or after the group the one before started at, so the control column and the rows are
/// written front to back, where key order costs a cache miss an entry in each.
pub(crate) fn build_index(entries: &[(Key, Loc)]) -> Vec<u8> {
    let mut slots = LEAST_SLOTS;
    while (entries.len() as u64) * LOAD_DENOMINATOR > slots * LOAD_NUMERATOR {
        slots *= 2;
    }
    let groups_mask = slots / GROUP as u64 - 1;
    let order = placing_order(entries, groups_mask);
    assert!(
        order.windows(2).all(|pair| entries[pair[0].1 as usize].0 != entries[pair[1].1 as usize].0),
        "an index table was given one key twice"
    );
    // alloc: the table built is the output, written whole
    let mut out = vec![0; INDEX_HEADER + slots as usize * (1 + ROW)];
    out[..8].copy_from_slice(&INDEX_MAGIC);
    out[8..12].copy_from_slice(&crate::FORMAT.to_le_bytes());
    out[16..24].copy_from_slice(&slots.to_le_bytes());
    out[24..32].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    let (control, rows) = out[INDEX_HEADER..].split_at_mut(slots as usize);
    control.fill(EMPTY);
    let lanes = matcher();
    for &(_, at) in &order {
        let (key, loc) = &entries[at as usize];
        let slot = free_slot(control, groups_mask, key, lanes);
        control[slot] = tag(key);
        let row = &mut rows[slot * ROW..(slot + 1) * ROW];
        row[..32].copy_from_slice(&key.0.0);
        row[32..36].copy_from_slice(&loc.pack.to_le_bytes());
        row[36] = loc.kind.code();
        row[37] = loc.form.code();
        row[40..48].copy_from_slice(&loc.offset.to_le_bytes());
        row[48..56].copy_from_slice(&loc.stored.to_le_bytes());
        row[56..64].copy_from_slice(&loc.raw.to_le_bytes());
    }
    out
}

/// Each entry's [`placing`] value and number, by that value and then by key: radix passes over
/// 8-byte pairs, the entries themselves only read. Equal values are rare, and a repeated key
/// ends up next to its repeat.
fn placing_order(entries: &[(Key, Loc)], groups_mask: u64) -> Vec<(u32, u32)> {
    assert!(u32::try_from(entries.len()).is_ok(), "an index table of {} entries", entries.len());
    assert!(groups_mask.count_ones() < 32, "an index table of {} groups", groups_mask + 1);
    let placed = entries.iter().zip(0..).map(|((key, _), at)| (placing(key, groups_mask), at));
    // alloc: once per index table built (a settle), 8 bytes an entry
    let mut order: Vec<(u32, u32)> = placed.collect();
    radix::sort_pairs(&mut order);
    let mut from = 0;
    while from < order.len() {
        let value = order[from].0;
        let to = from + order[from..].iter().take_while(|it| it.0 == value).count();
        if to - from > 1 {
            order[from..to].sort_unstable_by_key(|it| entries[it.1 as usize].0);
        }
        from = to;
    }
    order
}

/// `key`'s first group in the high bits, then the bits of key bytes 8..12 that fit below it.
fn placing(key: &Key, groups_mask: u64) -> u32 {
    let group_bits = groups_mask.count_ones();
    let bytes = &key.0.0;
    let rest = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    (((start(key) & groups_mask) as u32) << (32 - group_bits)) | (rest >> group_bits)
}

/// The lowest empty lane of the first group on `key`'s probe with one.
fn free_slot(control: &[u8], groups_mask: u64, key: &Key, lanes: Matcher) -> usize {
    let mut group = start(key) & groups_mask;
    for step in 1..=groups_mask + 1 {
        let at = group as usize * GROUP;
        let empty = lanes(group_at(control, at), EMPTY);
        if empty != 0 {
            return at + empty.trailing_zeros() as usize;
        }
        group = (group + step) & groups_mask;
    }
    unreachable!("an index table at most 7/8 full has an empty slot")
}

/// The key's first group, from its first eight bytes (a BLAKE3 hash is uniform already).
fn start(key: &Key) -> u64 {
    u64_at(&key.0.0, 0)
}

/// The key's tag, from a byte [`start`] does not read.
fn tag(key: &Key) -> u8 {
    key.0.0[8] & 0x7f
}

fn group_at(control: &[u8], at: usize) -> &[u8; GROUP] {
    control[at..at + GROUP].try_into().unwrap()
}

/// A checked table's header, kept with its mapping so a lookup need not count the control
/// column again.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    slots: usize,
    groups_mask: u64,
    count: u64,
    lanes: Matcher,
}

/// A mapped index table, its header and control column checked.
#[derive(Clone, Copy)]
pub(crate) struct Table<'a> {
    control: &'a [u8],
    rows: &'a [u8],
    groups_mask: u64,
    count: u64,
    lanes: Matcher,
}

impl<'a> Table<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Result<Table<'a>, &'static str> {
        if bytes.len() < INDEX_HEADER || bytes[..8] != INDEX_MAGIC {
            return Err("it does not start as an index of this store's format does");
        }
        if u32_at(bytes, 8) != crate::FORMAT {
            return Err("it is of another format of the store");
        }
        if bytes[12..16].iter().chain(&bytes[32..INDEX_HEADER]).any(|&byte| byte != 0) {
            return Err("its header has bytes set that no index sets");
        }
        let slots = u64_at(bytes, 16);
        let count = u64_at(bytes, 24);
        if !slots.is_power_of_two() || slots < LEAST_SLOTS {
            return Err("its header gives an impossible number of slots");
        }
        let len = slots.checked_mul(1 + ROW as u64).and_then(|it| it.checked_add(INDEX_HEADER as u64));
        if len != Some(bytes.len() as u64) {
            return Err("its length is not what its header says");
        }
        if count.checked_mul(LOAD_DENOMINATOR).is_none_or(|it| it > slots * LOAD_NUMERATOR) {
            return Err("its header gives more records than its slots may hold");
        }
        let (control, rows) = bytes[INDEX_HEADER..].split_at(slots as usize);
        if full_slots(control)? != count {
            return Err("its control column holds another number of records than its header");
        }
        Ok(Table { control, rows, groups_mask: slots / GROUP as u64 - 1, count, lanes: matcher() })
    }

    /// What [`Table::new`] found, to view the same bytes again without checking them again.
    pub(crate) fn shape(&self) -> Shape {
        Shape { slots: self.control.len(), groups_mask: self.groups_mask, count: self.count, lanes: self.lanes }
    }

    /// The table `shape` was taken from; `bytes` must be the bytes it was taken from.
    pub(crate) fn again(bytes: &'a [u8], shape: Shape) -> Table<'a> {
        let (control, rows) = bytes[INDEX_HEADER..].split_at(shape.slots);
        assert_eq!(rows.len(), shape.slots * ROW, "the bytes a table's shape was taken from");
        Table { control, rows, groups_mask: shape.groups_mask, count: shape.count, lanes: shape.lanes }
    }

    /// How many records it holds.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// How many slots it has: every slot number is below this.
    pub(crate) fn slots(&self) -> u64 {
        self.control.len() as u64
    }

    fn row(&self, slot: usize) -> &'a [u8] {
        &self.rows[slot * ROW..(slot + 1) * ROW]
    }

    /// The slot of `key` and its record's place. Groups are probed at triangular steps (g, g+1,
    /// g+3, g+6, ...), which over a power of two of groups visits each once.
    pub(crate) fn get(&self, key: &Key) -> Option<(u64, Result<Loc, &'static str>)> {
        let tag = tag(key);
        let mut group = start(key) & self.groups_mask;
        for step in 1..=self.groups_mask + 1 {
            let at = group as usize * GROUP;
            let control = group_at(self.control, at);
            let mut lanes = (self.lanes)(control, tag);
            while lanes != 0 {
                let slot = at + lanes.trailing_zeros() as usize;
                let row = self.row(slot);
                if row[..32] == key.0.0 {
                    return Some((slot as u64, decode_row(row)));
                }
                lanes &= lanes - 1;
            }
            if (self.lanes)(control, EMPTY) != 0 {
                return None;
            }
            group = (group + step) & self.groups_mask;
        }
        None
    }

    /// Every full slot, in slot order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, Key, Result<Loc, &'static str>)> + 'a {
        let table = *self;
        let control: &'a [u8] = self.control;
        control.iter().enumerate().filter(|(_, control)| **control != EMPTY).map(move |(slot, _)| {
            let row = table.row(slot);
            (slot as u64, Key(id_at(row, 0)), decode_row(row))
        })
    }
}

/// How many of `control`'s bytes are full, or why it is no control column: each byte must be
/// [`EMPTY`] or below it. Eight bytes at a time, as it runs at every open.
fn full_slots(control: &[u8]) -> Result<u64, &'static str> {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let mut empty = 0;
    for word in control.chunks_exact(8) {
        let word = u64_at(word, 0);
        let high = word & HIGH;
        // Each byte's low seven bits plus 0x7f stays within the byte and sets its top bit
        // exactly when they are not all zero.
        if high & ((word & LOW) + LOW) != 0 {
            return Err("its control column has a byte that is neither empty nor a tag");
        }
        empty += u64::from(high.count_ones());
    }
    Ok(control.len() as u64 - empty)
}

fn decode_row(row: &[u8]) -> Result<Loc, &'static str> {
    Ok(Loc {
        pack: u32_at(row, 32),
        kind: Kind::from_code(row[36]).ok_or("an index slot names no kind")?,
        form: Form::from_code(row[37]).ok_or("an index slot names no form")?,
        offset: u64_at(row, 40),
        stored: u64_at(row, 48),
        raw: u64_at(row, 56),
    })
}

/// The fastest group matcher this CPU has. Every one gives the lanes [`lanes_portable`] gives.
fn matcher() -> Matcher {
    #[cfg(target_arch = "x86_64")]
    if let Some(lanes) = sse2() {
        return lanes;
    }
    #[cfg(target_arch = "aarch64")]
    if let Some(lanes) = neon() {
        return lanes;
    }
    lanes_portable
}

/// The group as two words, compared a byte at a time without carries between bytes.
fn lanes_portable(group: &[u8; GROUP], byte: u8) -> u16 {
    let half = |at: usize| {
        const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
        let diff = u64_at(group, at) ^ (u64::from(byte) * 0x0101_0101_0101_0101);
        let zero = !(((diff & LOW) + LOW) | diff) & !LOW;
        // Moves the top bit of byte `i` to bit `56 + i`; no two products overlap, so none carry.
        ((zero >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56) as u16
    };
    half(0) | (half(8) << 8)
}

#[cfg(target_arch = "x86_64")]
fn sse2() -> Option<Matcher> {
    fn lanes(group: &[u8; GROUP], byte: u8) -> u16 {
        // SAFETY: handed out only after `sse2` was detected.
        unsafe { lanes_sse2(group, byte) }
    }
    is_x86_feature_detected!("sse2").then_some(lanes as Matcher)
}

/// # Safety
///
/// The CPU has SSE2: `sse2` hands it out only after detecting it.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lanes_sse2(group: &[u8; GROUP], byte: u8) -> u16 {
    use std::arch::x86_64::*;

    // SAFETY: `group` is 16 readable bytes, and the load takes any alignment.
    let group = unsafe { _mm_loadu_si128(group.as_ptr().cast()) };
    _mm_movemask_epi8(_mm_cmpeq_epi8(group, _mm_set1_epi8(byte as i8))) as u16
}

#[cfg(target_arch = "aarch64")]
fn neon() -> Option<Matcher> {
    fn lanes(group: &[u8; GROUP], byte: u8) -> u16 {
        // SAFETY: handed out only after `neon` was detected.
        unsafe { lanes_neon(group, byte) }
    }
    std::arch::is_aarch64_feature_detected!("neon").then_some(lanes as Matcher)
}

/// The comparison narrowed to 64 bits, four a lane, as NEON has no byte movemask.
///
/// # Safety
///
/// The CPU has NEON: `neon` hands it out only after detecting it.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn lanes_neon(group: &[u8; GROUP], byte: u8) -> u16 {
    use std::arch::aarch64::*;

    // SAFETY: `group` is 16 readable bytes, and the load takes any alignment.
    let group = unsafe { vld1q_u8(group.as_ptr()) };
    let equal = vceqq_u8(group, vdupq_n_u8(byte));
    let narrow = vshrn_n_u16::<4>(vreinterpretq_u16_u8(equal));
    nibble_lanes(vget_lane_u64::<0>(vreinterpret_u64_u8(narrow)))
}

/// The lane set of a mask with four equal bits a lane, lane `i` at bits `4i..4i + 4`.
#[cfg(any(target_arch = "aarch64", test))]
fn nibble_lanes(nibbles: u64) -> u16 {
    let x = nibbles & 0x1111_1111_1111_1111;
    let x = (x | (x >> 3)) & 0x0303_0303_0303_0303;
    let x = (x | (x >> 6)) & 0x000f_000f_000f_000f;
    let x = (x | (x >> 12)) & 0x0000_00ff_0000_00ff;
    ((x | (x >> 24)) & 0xffff) as u16
}

/// Every group matcher this CPU can run, the portable one first, for the differential tests.
#[cfg(test)]
pub(crate) fn paths() -> Vec<(&'static str, Matcher)> {
    let mut out: Vec<(&'static str, Matcher)> = vec![("portable", lanes_portable)];
    #[cfg(target_arch = "x86_64")]
    if let Some(lanes) = sse2() {
        out.push(("sse2", lanes));
    }
    #[cfg(target_arch = "aarch64")]
    if let Some(lanes) = neon() {
        out.push(("neon", lanes));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use blaze_ids::rng::SplitMix64 as Rng;
    use blaze_ids::ContentId;

    use super::*;

    fn random_key(rng: &mut Rng) -> Key {
        let mut bytes = [0; 32];
        for word in bytes.chunks_exact_mut(8) {
            word.copy_from_slice(&rng.next_u64().to_le_bytes());
        }
        Key(ContentId(bytes))
    }

    /// A key whose first eight bytes are `prefix`, so it starts on the same group as every
    /// other key with that prefix.
    fn clustered_key(rng: &mut Rng, prefix: u64) -> Key {
        let mut key = random_key(rng);
        key.0.0[..8].copy_from_slice(&prefix.to_le_bytes());
        key
    }

    fn loc(n: u64) -> Loc {
        Loc {
            pack: (n as u32) ^ 0x5a5a,
            kind: Kind::ALL[(n % Kind::ALL.len() as u64) as usize],
            form: Form::from_code((n % 6) as u8).unwrap(),
            offset: n * 16,
            stored: n + 1,
            raw: n * 3,
        }
    }

    fn with_locs(keys: Vec<Key>) -> Vec<(Key, Loc)> {
        keys.into_iter().zip(0..).map(|(key, n)| (key, loc(n))).collect()
    }

    fn random_entries(count: usize, seed: u64) -> Vec<(Key, Loc)> {
        let mut rng = Rng::new(seed);
        with_locs((0..count).map(|_| random_key(&mut rng)).collect())
    }

    /// The same key with its last byte changed: the same group and tag, so only the full
    /// comparison tells them apart.
    fn neighbour(key: &Key) -> Key {
        let mut key = *key;
        key.0.0[31] ^= 1;
        key
    }

    fn finds_exactly(entries: &[(Key, Loc)], absent: &[Key]) {
        let bytes = build_index(entries);
        let table = Table::new(&bytes).unwrap();
        assert_eq!(table.count(), entries.len() as u64);
        assert!(table.slots().is_power_of_two() && table.slots() >= LEAST_SLOTS);
        assert!(table.count() * LOAD_DENOMINATOR <= table.slots() * LOAD_NUMERATOR);
        let present: HashSet<Key> = entries.iter().map(|(key, _)| *key).collect();
        let mut slots = HashSet::new();
        for (key, loc) in entries {
            let (slot, found) = table.get(key).expect("an entry's key is found");
            assert!(slot < table.slots());
            assert_eq!(found, Ok(*loc));
            assert!(slots.insert(slot), "two keys share slot {slot}");
            let other = neighbour(key);
            if !present.contains(&other) {
                assert_eq!(table.get(&other), None);
            }
        }
        for key in absent {
            assert!(!present.contains(key));
            assert_eq!(table.get(key), None);
        }
    }

    #[test]
    fn every_key_is_found_and_no_other() {
        for (seed, count) in [0, 1, 15, 16, 17, 1000, 3000, 100_000].into_iter().enumerate() {
            let entries = random_entries(count, seed as u64);
            let mut rng = Rng::new(1000 + seed as u64);
            let absent: Vec<Key> = (0..1000).map(|_| random_key(&mut rng)).collect();
            finds_exactly(&entries, &absent);
        }
    }

    #[test]
    fn the_load_stays_at_most_seven_eighths() {
        let at = |count| Table::new(&build_index(&random_entries(count, 7))).unwrap().slots();
        assert_eq!(at(0), LEAST_SLOTS);
        assert_eq!(at(896), 1024);
        assert_eq!(at(897), 2048);
        assert_eq!(at(1792), 2048);
        assert_eq!(at(1793), 4096);
    }

    #[test]
    fn clustered_keys_overflow_across_groups_and_are_still_found() {
        let mut rng = Rng::new(11);
        let prefix = rng.next_u64();
        let keys: Vec<Key> = (0..3000).map(|_| clustered_key(&mut rng, prefix)).collect();
        let absent: Vec<Key> = (0..1000).map(|_| clustered_key(&mut rng, prefix)).collect();
        finds_exactly(&with_locs(keys), &absent);

        let mut keys: Vec<Key> = (0..500).map(|_| clustered_key(&mut rng, prefix)).collect();
        for key in &mut keys {
            key.0.0[8] = 0x2a;
        }
        let mut absent: Vec<Key> = (0..500).map(|_| clustered_key(&mut rng, prefix)).collect();
        for key in &mut absent {
            key.0.0[8] = 0x2a;
        }
        finds_exactly(&with_locs(keys), &absent);
    }

    #[test]
    fn the_bytes_depend_only_on_the_set_of_entries() {
        let entries = random_entries(3000, 21);
        let bytes = build_index(&entries);
        let mut rng = Rng::new(22);
        for _ in 0..4 {
            let mut shuffled = entries.clone();
            rng.shuffle(&mut shuffled);
            assert!(build_index(&shuffled) == bytes);
        }
        let reversed: Vec<(Key, Loc)> = entries.iter().rev().copied().collect();
        assert!(build_index(&reversed) == bytes);
    }

    /// The table placed in key order, as it was before entries were placed by group.
    fn build_in_key_order(entries: &[(Key, Loc)]) -> Vec<u8> {
        let mut entries = entries.to_vec();
        entries.sort_unstable_by_key(|(key, _)| *key);
        let mut slots = LEAST_SLOTS;
        while (entries.len() as u64) * LOAD_DENOMINATOR > slots * LOAD_NUMERATOR {
            slots *= 2;
        }
        let mut out = vec![0; INDEX_HEADER + slots as usize * (1 + ROW)];
        out[..INDEX_HEADER].copy_from_slice(&build_index(&[])[..INDEX_HEADER]);
        out[16..24].copy_from_slice(&slots.to_le_bytes());
        out[24..32].copy_from_slice(&(entries.len() as u64).to_le_bytes());
        let (control, rows) = out[INDEX_HEADER..].split_at_mut(slots as usize);
        control.fill(EMPTY);
        let groups_mask = slots / GROUP as u64 - 1;
        for (key, loc) in &entries {
            let slot = free_slot(control, groups_mask, key, lanes_portable);
            control[slot] = tag(key);
            let row = &mut rows[slot * ROW..(slot + 1) * ROW];
            row[..32].copy_from_slice(&key.0.0);
            row[32..36].copy_from_slice(&loc.pack.to_le_bytes());
            row[36] = loc.kind.code();
            row[37] = loc.form.code();
            row[40..48].copy_from_slice(&loc.offset.to_le_bytes());
            row[48..56].copy_from_slice(&loc.stored.to_le_bytes());
            row[56..64].copy_from_slice(&loc.raw.to_le_bytes());
        }
        out
    }

    /// Placing by group answers every lookup as placing in key order did: the same header,
    /// the same entries found, the same keys absent; only which slot holds an entry differs.
    #[test]
    fn placing_by_group_answers_as_placing_by_key_did() {
        let mut rng = Rng::new(61);
        let prefix = rng.next_u64();
        let mut sets: Vec<Vec<(Key, Loc)>> =
            [0, 1, 17, 896, 897, 5000, 60_000].into_iter().map(|count| random_entries(count, 60 + count as u64)).collect();
        sets.push(with_locs((0..2000).map(|_| clustered_key(&mut rng, prefix)).collect()));
        let absent: Vec<Key> = (0..2000).map(|_| random_key(&mut rng)).collect();
        for entries in &sets {
            let (new, old) = (build_index(entries), build_in_key_order(entries));
            assert_eq!(new.len(), old.len());
            assert_eq!(new[..INDEX_HEADER], old[..INDEX_HEADER]);
            let (new, old) = (Table::new(&new).unwrap(), Table::new(&old).unwrap());
            for (key, loc) in entries {
                assert_eq!(new.get(key).map(|it| it.1), Some(Ok(*loc)));
                assert_eq!(old.get(key).map(|it| it.1), Some(Ok(*loc)));
            }
            let neighbours: Vec<Key> = entries.iter().map(|(key, _)| neighbour(key)).collect();
            for key in absent.iter().chain(&neighbours) {
                assert_eq!(new.get(key).is_some(), old.get(key).is_some());
            }
        }
    }

    /// Entries are placed group by group, each once, in the order of the placing value and
    /// then of the key.
    #[test]
    fn the_placing_order_never_goes_back_a_group() {
        let entries = random_entries(20_000, 62);
        let groups_mask = (32 << 10) / GROUP as u64 - 1;
        let order = placing_order(&entries, groups_mask);
        let mut seen: Vec<u32> = order.iter().map(|it| it.1).collect();
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..entries.len() as u32));
        let place = |it: &(u32, u32)| {
            let key = entries[it.1 as usize].0;
            assert_eq!(it.0, placing(&key, groups_mask));
            (start(&key) & groups_mask, it.0, key)
        };
        assert!(order.windows(2).all(|pair| place(&pair[0]) < place(&pair[1])));
    }

    /// Keys alike in every byte the placing value reads are still placed by key, so the bytes
    /// depend only on the set however they come.
    #[test]
    fn keys_tied_in_the_placing_value_are_placed_by_key() {
        let mut rng = Rng::new(63);
        let prefix = rng.next_u64();
        let keys: Vec<Key> = (0..300)
            .map(|_| {
                let mut key = clustered_key(&mut rng, prefix);
                key.0.0[8..12].copy_from_slice(&[1, 2, 3, 4]);
                key
            })
            .collect();
        let mut entries = with_locs(keys);
        entries.extend(random_entries(500, 64));
        let bytes = build_index(&entries);
        for _ in 0..4 {
            rng.shuffle(&mut entries);
            assert!(build_index(&entries) == bytes);
        }
        finds_exactly(&entries, &[]);
    }

    #[test]
    #[should_panic(expected = "one key twice")]
    fn a_repeated_key_is_refused() {
        let mut entries = random_entries(10, 23);
        entries.push((entries[3].0, loc(99)));
        build_index(&entries);
    }

    fn naive(group: &[u8; GROUP], byte: u8) -> u16 {
        (0..GROUP).filter(|&lane| group[lane] == byte).fold(0, |lanes, lane| lanes | (1 << lane))
    }

    #[test]
    fn every_path_gives_the_portable_lanes() {
        let paths = paths();
        assert_eq!(paths[0].0, "portable");
        let mut rng = Rng::new(31);
        let mut groups = vec![[EMPTY; GROUP], [0; GROUP], [0x2a; GROUP], [0x7f; GROUP]];
        for _ in 0..3000 {
            let mut group = [0; GROUP];
            let distinct = [2, 4, 0x80][rng.below(3) as usize];
            for byte in &mut group {
                *byte = if rng.one_in(3) { EMPTY } else { rng.below(distinct) as u8 };
            }
            groups.push(group);
        }
        for group in &groups {
            let mut bytes = vec![EMPTY, 0, 1, 0x2a, 0x7f];
            bytes.extend_from_slice(group);
            bytes.extend((0..4).map(|_| rng.below(0x80) as u8));
            for byte in bytes {
                let expected = naive(group, byte);
                assert_eq!(lanes_portable(group, byte), expected, "{group:02x?} {byte:#x}");
                for (name, lanes) in &paths {
                    assert_eq!(lanes(group, byte), expected, "{name}: {group:02x?} {byte:#x}");
                }
            }
        }
    }

    #[test]
    fn nibble_lanes_inverts_the_narrowed_mask() {
        let mut rng = Rng::new(32);
        for lanes in [0, 1, 0x8000, 0xffff, 0x5555, 0xaaaa].into_iter().chain((0..2000).map(|_| rng.next_u64() as u16)) {
            let nibbles = (0..GROUP)
                .filter(|&lane| lanes & (1 << lane) != 0)
                .fold(0u64, |mask, lane| mask | (0xf << (4 * lane)));
            assert_eq!(nibble_lanes(nibbles), lanes, "{lanes:#06x}");
        }
    }

    #[test]
    fn iter_lists_every_entry_once_in_slot_order() {
        for (seed, count) in [(41, 0), (42, 17), (43, 3000)] {
            let entries = random_entries(count, seed);
            let bytes = build_index(&entries);
            let table = Table::new(&bytes).unwrap();
            let listed: Vec<(u64, Key, Result<Loc, &str>)> = table.iter().collect();
            assert_eq!(listed.len(), count);
            assert!(listed.windows(2).all(|pair| pair[0].0 < pair[1].0));
            for (slot, key, loc) in &listed {
                assert!(*slot < table.slots());
                assert_eq!(table.get(key), Some((*slot, *loc)));
            }
            let mut keys: Vec<Key> = listed.iter().map(|(_, key, _)| *key).collect();
            keys.sort();
            let mut expected: Vec<Key> = entries.iter().map(|(key, _)| *key).collect();
            expected.sort();
            assert_eq!(keys, expected);
        }
    }

    #[test]
    fn new_refuses_what_no_build_writes() {
        let entries = random_entries(1000, 51);
        let bytes = build_index(&entries);
        assert!(Table::new(&bytes).is_ok());
        let control = INDEX_HEADER..INDEX_HEADER + u64_at(&bytes, 16) as usize;
        let empty = control.clone().find(|&at| bytes[at] == EMPTY).unwrap();
        let full = control.clone().find(|&at| bytes[at] != EMPTY).unwrap();

        for len in [0, 10, INDEX_HEADER, INDEX_HEADER + 1024, bytes.len() - ROW, bytes.len() - 1] {
            assert!(Table::new(&bytes[..len]).is_err(), "cut to {len}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Table::new(&longer).is_err());

        for at in [empty, full] {
            let mut changed = bytes.clone();
            changed[at] = 0x81;
            assert!(Table::new(&changed).is_err(), "0x81 at {at}");
        }
        let mut filled = bytes.clone();
        filled[empty] = 0x05;
        assert!(Table::new(&filled).is_err());
        let mut emptied = bytes.clone();
        emptied[full] = EMPTY;
        assert!(Table::new(&emptied).is_err());

        for count in [999, 1001] {
            let mut changed = bytes.clone();
            changed[24..32].copy_from_slice(&(count as u64).to_le_bytes());
            assert!(Table::new(&changed).is_err(), "count {count}");
        }

        let mut format = bytes.clone();
        format[8..12].copy_from_slice(&(crate::FORMAT + 1).to_le_bytes());
        assert!(Table::new(&format).is_err());
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert!(Table::new(&magic).is_err());
        let mut reserved = bytes.clone();
        reserved[40] = 1;
        assert!(Table::new(&reserved).is_err());
        let mut slots = bytes.clone();
        slots[16..24].copy_from_slice(&512u64.to_le_bytes());
        assert!(Table::new(&slots).is_err());
    }
}
