//! The identity of a set of keyed entries, kept as entries are set and removed
//! (docs/v2/plan/design/patch-in-place.md): what a link is given, as one id, without hashing
//! every entry each link.
//!
//! A Merkle tree over the keys' bits. The subtree of the keys that share a prefix hashes to
//! zeros when it holds no entry, to its entry's leaf when it holds one, and to the hash of its
//! two halves (the next bit 0, the next bit 1) when it holds more. The root binds the top to
//! the number of entries. The tree's shape is a function of the set alone, never of the order
//! entries came in, so the root kept through any edits is the root [`Keyed::of`] makes of the
//! same entries. Setting or removing one entry hashes its leaf and one node for each bit its
//! key shares with a neighbour: about `log2(n)` nodes for keys that are hashes.
//!
//! It is a sound identity because it is a hash of the whole set: a leaf is the hash of its
//! key and value, in a domain of its own, so by the collision resistance of the hash two sets
//! with one root have the same count and the same top, so the same halves all the way down, so
//! the same keys with the same values. A sum or an XOR of the entries' hashes would update as
//! cheaply and is not one: entries summing to a chosen value are found by linear algebra (XOR)
//! or by a generalized birthday search (a sum of words).

#![forbid(unsafe_code)]

use std::{alloc::Global, collections::BTreeMap};

use blaze_hash::HashMap;
use blaze_ids::Hasher;

/// A key, a value, a leaf, a node or a root.
pub type Hash = [u8; 32];

const LEAF: &str = "blaze v2 link keyed leaf 1";
const NODE: &str = "blaze v2 link keyed node 1";
const ROOT: &str = "blaze v2 link keyed set 1";

/// The hash of a subtree that holds no entry.
const EMPTY: Hash = [0; 32];

/// The bits of a key.
const BITS: usize = 256;

fn leaf(key: &Hash, value: &Hash) -> Hash {
    let mut hasher = Hasher::new(LEAF);
    hasher.update(key).update(value);
    hasher.finish().0
}

fn node(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Hasher::new(NODE);
    hasher.update(left).update(right);
    hasher.finish().0
}

/// Bit `at` of `key`, the most significant of its first byte first.
fn bit(key: &Hash, at: usize) -> bool {
    key[at / 8] >> (7 - at % 8) & 1 == 1
}

/// The least and the greatest key that share `key`'s first `depth` bits.
fn bounds(key: &Hash, depth: usize) -> (Hash, Hash) {
    let (mut low, mut high) = (*key, *key);
    let (byte, rest) = (depth / 8, depth % 8);
    if byte < 32 {
        let after = 0xffu8 >> rest;
        low[byte] &= !after;
        high[byte] |= after;
        for at in byte + 1..32 {
            low[at] = 0;
            high[at] = 0xff;
        }
    }
    (low, high)
}

/// What a subtree holds.
enum Under {
    Nothing,
    /// One entry: its leaf.
    One(Hash),
    Many,
}

/// A set of entries by key and its Merkle tree. The invariant: a node is kept for exactly
/// each prefix that two entries or more share, under the prefix's depth and least key, and it
/// is the hash of its two halves.
pub struct Keyed {
    /// Each entry's value and leaf, by key.
    // BTreeMap: the entries under a prefix are a range of it.
    entries: BTreeMap<Hash, (Hash, Hash)>,
    nodes: HashMap<(u16, Hash), Hash, Global>,
    /// The bytes hashed since it was made: 64 a leaf or a node.
    hashed: u64,
    /// The planted bug of `the_oracle_catches_a_removed_entry_left_in_the_root`: a removal
    /// drops the entry and leaves the nodes above it.
    #[cfg(test)]
    pub(crate) plant_keeps_removed: bool,
}

impl Default for Keyed {
    fn default() -> Keyed {
        Keyed::new()
    }
}

impl Keyed {
    /// The empty set.
    pub fn new() -> Keyed {
        Keyed {
            // alloc: none until an entry is set, then kept as long as the set
            entries: BTreeMap::new(),
            // alloc: none until two entries are set, then kept as long as the set
            nodes: HashMap::new_in(Global),
            hashed: 0,
            #[cfg(test)]
            plant_keeps_removed: false,
        }
    }

    /// The set of `entries`, every leaf and node hashed: what a set kept through edits is
    /// checked against. A key given twice keeps its last value.
    pub fn of(entries: impl IntoIterator<Item = (Hash, Hash)>) -> Keyed {
        let mut made = Keyed::new();
        for (key, value) in entries {
            made.entries.insert(key, (value, leaf(&key, &value)));
        }
        made.hashed = 64 * made.entries.len() as u64;
        // alloc: once per set made whole, its leaves in key order
        let leaves: Vec<(Hash, Hash)> = made.entries.iter().map(|(key, it)| (*key, it.1)).collect();
        made.build(&leaves, 0);
        made
    }

    /// The hash of the subtree of `leaves` (sorted, sharing their first `depth` bits), its
    /// nodes kept.
    fn build(&mut self, leaves: &[(Hash, Hash)], depth: usize) -> Hash {
        match leaves {
            [] => EMPTY,
            [(_, leaf)] => *leaf,
            [(first, _), ..] => {
                let split = leaves.partition_point(|(key, _)| !bit(key, depth));
                let left = self.build(&leaves[..split], depth + 1);
                let right = self.build(&leaves[split..], depth + 1);
                let hash = node(&left, &right);
                self.hashed += 64;
                self.nodes.insert((depth as u16, bounds(first, depth).0), hash);
                hash
            }
        }
    }

    /// How many entries the set holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set holds no entry.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value of `key`, if the set holds it.
    pub fn get(&self, key: &Hash) -> Option<&Hash> {
        self.entries.get(key).map(|it| &it.0)
    }

    /// How many nodes are kept: one for each prefix two entries share.
    pub fn nodes(&self) -> usize {
        self.nodes.len()
    }

    /// The bytes hashed since the set was made.
    pub fn hashed(&self) -> u64 {
        self.hashed
    }

    /// The bytes the set holds: its entries and nodes.
    pub fn held(&self) -> u64 {
        (self.entries.len() * 96 + self.nodes.len() * 66) as u64
    }

    /// What the subtree of the keys sharing `key`'s first `depth` bits holds.
    fn under(&self, key: &Hash, depth: usize) -> Under {
        let (low, high) = bounds(key, depth);
        let mut found = self.entries.range(low..=high);
        match (found.next(), found.next()) {
            (None, _) => Under::Nothing,
            (Some((_, (_, leaf))), None) => Under::One(*leaf),
            _ => Under::Many,
        }
    }

    /// The hash of that subtree. A subtree of two entries with no node kept breaks the
    /// invariant and panics, naming it.
    fn hash_under(&self, key: &Hash, depth: usize) -> Hash {
        match self.under(key, depth) {
            Under::Nothing => EMPTY,
            Under::One(leaf) => leaf,
            Under::Many => {
                let low = bounds(key, depth).0;
                match self.nodes.get(&(depth as u16, low)) {
                    Some(hash) => *hash,
                    None => panic!(
                        "a keyed set holds two entries under the first {depth} bits of {low:02x?} and no node for them"
                    ),
                }
            }
        }
    }

    /// Hashes again the nodes on `key`'s path, after its entry was set or removed: the path
    /// is a node for as deep as another entry shares the prefix, and no node below.
    fn refresh(&mut self, key: &Hash) {
        let mut deepest = None;
        for depth in 0..BITS {
            if !matches!(self.under(key, depth), Under::Many) {
                break;
            }
            deepest = Some(depth);
        }
        // A prefix that two entries shared before and one or none does now keeps no node.
        // Such prefixes are the ones just below the deepest, each longer than the last.
        for depth in deepest.map_or(0, |it| it + 1)..BITS {
            if self.nodes.remove(&(depth as u16, bounds(key, depth).0)).is_none() {
                break;
            }
        }
        let Some(deepest) = deepest else { return };
        for depth in (0..=deepest).rev() {
            let (low, high) = bounds(key, depth);
            // The half whose next bit is 0 holds `low`, the other `high`.
            let left = self.hash_under(&low, depth + 1);
            let right = self.hash_under(&high, depth + 1);
            self.hashed += 64;
            self.nodes.insert((depth as u16, low), node(&left, &right));
        }
    }

    /// `key` now has `value`; whether that changed the set.
    pub fn set(&mut self, key: Hash, value: Hash) -> bool {
        if self.entries.get(&key).is_some_and(|it| it.0 == value) {
            return false;
        }
        self.hashed += 64;
        self.entries.insert(key, (value, leaf(&key, &value)));
        self.refresh(&key);
        true
    }

    /// `key` has no entry any more; whether it had one.
    pub fn remove(&mut self, key: &Hash) -> bool {
        if self.entries.remove(key).is_none() {
            return false;
        }
        #[cfg(test)]
        {
            if self.plant_keeps_removed {
                return true;
            }
        }
        self.refresh(key);
        true
    }

    /// The set's identity: of how many entries it holds and its top.
    pub fn root(&self) -> Hash {
        let top = self.hash_under(&EMPTY, 0);
        let mut hasher = Hasher::new(ROOT);
        hasher.update(&(self.entries.len() as u64).to_le_bytes()).update(&top);
        hasher.finish().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    /// One edit of a set.
    #[derive(Clone, Debug)]
    enum Op {
        Set { key: Hash, value: u8 },
        Remove(Hash),
    }

    /// A key of a small space whose members share prefixes of every length: two bits at the
    /// top, two in the middle, two at the very end.
    #[hegel::composite]
    fn key(tc: &hegel::TestCase) -> Hash {
        let mut key = [0; 32];
        key[0] = tc.draw(gs::integers::<u8>().max_value(3)) << 6;
        key[17] = tc.draw(gs::integers::<u8>().max_value(3)) << 3;
        key[31] = tc.draw(gs::integers::<u8>().max_value(3));
        key
    }

    #[hegel::composite]
    fn op(tc: &hegel::TestCase) -> Op {
        match tc.draw(gs::integers::<u8>().max_value(2)) {
            0 => Op::Remove(tc.draw(key())),
            _ => Op::Set { key: tc.draw(key()), value: tc.draw(gs::integers::<u8>().max_value(3)) },
        }
    }

    /// `ops` applied to a set kept through them and to a plain map; `Err` naming the first
    /// step after which the kept root is not the root of the map's entries hashed whole.
    /// `plant` leaves a removed entry's nodes.
    fn kept_is_whole(ops: &[Op], plant: bool) -> Result<(), String> {
        let mut model: BTreeMap<Hash, Hash> = BTreeMap::new();
        let mut kept = Keyed::new();
        kept.plant_keeps_removed = plant;
        for (step, op) in ops.iter().enumerate() {
            match op {
                Op::Set { key, value } => {
                    let changed = model.insert(*key, [*value; 32]) != Some([*value; 32]);
                    assert_eq!(kept.set(*key, [*value; 32]), changed, "step {step} ({op:?})");
                }
                Op::Remove(key) => {
                    let had = model.remove(key).is_some();
                    assert_eq!(kept.remove(key), had, "step {step} ({op:?})");
                }
            }
            let whole = Keyed::of(model.iter().map(|(key, value)| (*key, *value)));
            if kept.root() != whole.root() {
                return Err(format!(
                    "after step {step} ({op:?}): the kept root differs from the root of the {} entries",
                    model.len()
                ));
            }
            if kept.nodes() != whole.nodes() {
                return Err(format!(
                    "after step {step} ({op:?}): {} nodes kept, {} in the set made whole: not canonical",
                    kept.nodes(),
                    whole.nodes()
                ));
            }
        }
        Ok(())
    }

    /// The refinement: after any sequence of sets and removals, the root kept by hashing
    /// only the edited entries' paths is the root of the same entries hashed whole, and so
    /// are the nodes kept.
    #[hegel::test]
    fn a_keyed_set_kept_by_edits_is_the_set_made_whole(tc: hegel::TestCase) {
        let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(24));
        if let Err(why) = kept_is_whole(&ops, false) {
            panic!("{why}");
        }
    }

    fn keyed(top: u8, end: u8) -> Hash {
        let mut key = [0; 32];
        (key[0], key[31]) = (top << 6, end);
        key
    }

    /// A control of [`a_keyed_set_kept_by_edits_is_the_set_made_whole`]: with a removal that
    /// leaves the nodes above its entry, the root still says the removed entry, and the
    /// oracle says at which step.
    #[test]
    fn the_oracle_catches_a_removed_entry_left_in_the_root() {
        let ops = vec![
            Op::Set { key: keyed(0, 0), value: 1 },
            Op::Set { key: keyed(0, 1), value: 2 },
            Op::Set { key: keyed(3, 0), value: 3 },
            Op::Remove(keyed(0, 1)),
        ];
        assert_eq!(kept_is_whole(&ops, false), Ok(()));
        let why = kept_is_whole(&ops, true).expect_err("the oracle missed a removal not hashed");
        assert!(why.contains("after step 3"), "{why}");
    }

    /// The root is of the set, not of how it was reached, and says each key, each value and
    /// how many entries there are.
    #[test]
    fn the_root_is_of_the_entries_alone() {
        let (a, b, c) = (keyed(0, 0), keyed(0, 1), keyed(2, 0));
        let mut one = Keyed::new();
        for key in [a, b, c] {
            one.set(key, [7; 32]);
        }
        let mut other = Keyed::new();
        for key in [c, a, b, keyed(1, 1)] {
            other.set(key, [7; 32]);
        }
        other.remove(&keyed(1, 1));
        assert_eq!(one.root(), other.root());
        assert_eq!(one.nodes(), other.nodes());
        assert_ne!(Keyed::of([(a, [7; 32])]).root(), Keyed::of([(b, [7; 32])]).root(), "the key");
        assert_ne!(Keyed::of([(a, [7; 32])]).root(), Keyed::of([(a, [8; 32])]).root(), "the value");
        assert_ne!(Keyed::new().root(), Keyed::of([(EMPTY, EMPTY)]).root(), "the count");
        assert_eq!(Keyed::of([]).nodes(), 0);
    }

    /// One entry set among many hashes its leaf and the nodes of its path, not the set: the
    /// bytes hashed among 10,000 entries are within a small multiple of those among 100.
    #[test]
    fn one_entry_set_hashes_its_path() {
        let entry = |at: u32| (blaze_ids::ContentId::of("keyed test", &at.to_le_bytes()).0, [1; 32]);
        let mut cost = [0u64; 2];
        for (slot, count) in [100u32, 10_000].into_iter().enumerate() {
            let mut kept = Keyed::of((0..count).map(entry));
            let before = kept.hashed();
            let (key, _) = entry(7);
            assert!(kept.set(key, [2; 32]));
            cost[slot] = kept.hashed() - before;
            assert_eq!(kept.root(), Keyed::of((0..count).map(entry).map(|it| match it.0 == key {
                true => (key, [2; 32]),
                false => it,
            })).root());
        }
        assert!(cost[0] <= 64 * 40 && cost[1] <= 64 * 40, "{cost:?}: a leaf and a node a shared bit");
    }
}
