//! Keys that hold a digest, hashed by one word of it: [`DigestState`], a table's hasher that
//! takes a [`DigestKey`] and no other key.
//!
//! A digest's bits are uniform already, so hashing one again is work for nothing: its first
//! eight bytes are as good a hash as any mixer makes of all 32. Three hashers here take that
//! word, each for another kind of key:
//!
//! - [`FixedState`](crate::FixedState) over a [`PlainKey`](crate::PlainKey) whose
//!   [`UNIFORM`](crate::PlainKey::UNIFORM) is set (`ContentId`, `Fingerprint`): a key that is
//!   all digest. Every table here hashes through [`KeyHash`] (the single-owner, the ordered and
//!   the shared ones alike), so such a key costs one load on any of them.
//! - [`DigestState`] over a [`DigestKey`]: a key that holds a digest beside other fields (an
//!   engine cell's kind, subject digest and cell). Its `Hash` feeds a `Hasher` field by field,
//!   the digest's 32 bytes and their length through foldhash's long path; its `DigestKey`
//!   names the digest's word ([`digest_word`]) and folds each other field in with one multiply
//!   ([`fold_part`]). A table on `DigestState` of a key with no `DigestKey` impl does not
//!   compile: `DigestState` is no `BuildHasher`, so nothing is hashed by an accident of layout.
//! - [`BuildKeyHasher`](crate::BuildKeyHasher) over any `Hash` key: the first eight bytes of
//!   each write, xored. It hashes whatever it is handed, a key that is no digest too, and a
//!   write shorter than a word adds nothing to it; `DigestState` is its typed form.
//!
//! A digest key's word is the same under `DigestState` and, for a key that is all digest, under
//! `FixedState`'s plain path: both are the first eight bytes, little-endian.
//!
//! # HashDoS
//!
//! `DigestState` has no seed: a key's word is its digest's, the same in every process, and
//! [`fold_part`]'s constants are public. Who chooses a digest's input chooses its low bits by
//! trying inputs, and so its bucket: to put `m` keys in one of a table's `2^b` index groups
//! takes about `m * 2^b` digests of texts the attacker writes, and then each probe for one of
//! them walks the `m`. The digest keys blaze tables hold are digests of the build's own inputs
//! (the workspace, its dependencies, the toolchain): code whose build scripts and proc macros
//! already run in the build, sandboxed under a deadline, and can spend its time directly, so a
//! crafted collision adds nothing a dependency cannot already do. A table whose keys come from
//! outside the build's inputs (another user's bytes over a socket, a record a reader did not
//! make) takes [`SipKey`](crate::SipKey), its key handed in (rule 75); a seed here would not do,
//! as an odd multiply or an xor keeps two words' low-bit collision, and a seed that mixes is
//! the hash this type exists not to compute. Each table's cap (rule 18) bounds what a crowded
//! group costs.

use crate::{KeyHash, plain::mix};

/// A key that holds a digest: it names one word of itself that is uniformly distributed over
/// the keys of a table, and under [`DigestState`] that word is its hash.
///
/// The contract, a correctness one as `Hash`'s agreement with `Eq` is:
///
/// - equal keys (`==`) have equal words;
/// - a probe a table is asked with (a `Q` that is [`Equivalent`](crate::Equivalent) to the
///   key) is a `DigestKey` whose word equals that of each key it is equivalent to.
///
/// And the one that makes it fast: the word is the digest's [`digest_word`] with each other
/// field of the key folded in by [`fold_part`], in a fixed order. A word that leaves a field out
/// is not wrong, only slow: keys equal but for that field share a bucket.
pub trait DigestKey: Eq {
    /// The key's hash: its digest's word with its other fields folded in.
    fn digest_word(&self) -> u64;
}

/// The hasher of a table of [`DigestKey`]s: a key's hash is its [`DigestKey::digest_word`],
/// taken as it is. Zero-sized, `Copy` and `Default`, as [`FixedState`](crate::FixedState) is;
/// not a `BuildHasher`, so a table on it holds only keys that say which word of them is the
/// hash. `MapOpts { hasher: DigestState, alloc, capacity: None }`.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct DigestState;

// Not a specialization of the base impl (`plain`): that one is for a `BuildHasher`, and
// `DigestState` is not one, so the two impls do not overlap.
impl<K: DigestKey + ?Sized> KeyHash<K> for DigestState {
    #[inline(always)]
    fn hash_key(&self, key: &K) -> u64 {
        key.digest_word()
    }
}

/// A digest's word: its first eight bytes, little-endian, as
/// [`hash_plain`](crate::hash_plain) takes a uniform key's, so a digest hashes alike under
/// `DigestState` and `FixedState`. A digest of fewer than eight bytes does not compile.
#[inline(always)]
pub const fn digest_word<const N: usize>(digest: &[u8; N]) -> u64 {
    const { assert!(N >= 8, "a digest's word is read from its first eight bytes") };
    match digest.first_chunk::<8>() {
        Some(word) => u64::from_le_bytes(*word),
        None => unreachable!(),
    }
}

/// `word` with `part`, a field of the key beside its digest, folded in by one 64 by 64-bit
/// multiply (`plain`'s mixer): keys of one digest that differ in `part` get words as far apart
/// as two digests' do, both in the low bits a table places by and the top seven it tags by.
/// Folds are ordered: folding `a` then `b` is not folding `b` then `a`, so two fields that trade
/// values do not collide. A key of one digest and nothing else folds nothing.
#[inline(always)]
pub const fn fold_part(word: u64, part: u64) -> u64 {
    mix(word ^ part)
}

#[cfg(test)]
mod tests {
    use core::fmt;
    use std::{collections::BTreeMap, vec::Vec};

    use blaze_own::Alloc;
    use blaze_sim::{Broken, Config, Tape, check, controls};

    use super::{DigestKey, DigestState, digest_word, fold_part};
    use crate::{FixedState, KeyHash, Map, MapOpts, OrderMap, OrderMapOpts, PlainKey, hash_plain};

    /// The test's allocator (D102): the process heap's handle, never `Global`.
    fn heap() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    /// A digest, as `blaze_ids::ContentId` is: plain, uniform, and a digest key.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
    #[repr(transparent)]
    struct Digest([u8; 32]);

    // SAFETY: a transparent wrapper of a plain array with derived `Eq`; lifetime-free, as a
    // specialization trait's impl must be.
    unsafe impl PlainKey for Digest {
        const UNIFORM: bool = true;
    }

    impl DigestKey for Digest {
        fn digest_word(&self) -> u64 {
            digest_word(&self.0)
        }
    }

    /// splitmix64: a fixed stream of uniform words, for digests a test needs uniform.
    fn splitmix(seed: u64) -> u64 {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// The `at`th digest of the stream `seed` names: four uniform words.
    fn digest_of(seed: u64, at: u64) -> Digest {
        let mut bytes = [0u8; 32];
        for (word, chunk) in (0u64..).zip(bytes.chunks_exact_mut(8)) {
            let base = seed ^ splitmix(at.wrapping_mul(4).wrapping_add(word));
            chunk.copy_from_slice(&splitmix(base).to_le_bytes());
        }
        Digest(bytes)
    }

    /// A digest's hash is its first word, little-endian, under `DigestState` and under
    /// `FixedState`'s plain path alike: a table of `ContentId`s hashes alike on either.
    #[test]
    fn a_digest_is_its_first_word_on_both_hashers() {
        check(Config::cases(200), |tape: &Tape| {
            let mut bytes = [0u8; 32];
            tape.fill(&mut bytes, "digest");
            let digest = Digest(bytes);
            let first = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            assert_eq!(KeyHash::<Digest>::hash_key(&DigestState, &digest), first);
            assert_eq!(KeyHash::<Digest>::hash_key(&FixedState, &digest), first);
            assert_eq!(hash_plain(&digest), first);
        });
    }

    /// An id with a digest in it, the shape of `blaze_engine::CellId` (a kind, a subject
    /// digest, a 16-byte cell): no `PlainKey`, so on `FixedState` it is hashed by `Hash`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
    struct Id {
        kind: u8,
        subject: Digest,
        cell: [u8; 16],
    }

    impl DigestKey for Id {
        fn digest_word(&self) -> u64 {
            id_word(self, WordPlant::None)
        }
    }

    /// A cell's two words, little-endian.
    fn halves(cell: &[u8; 16]) -> (u64, u64) {
        let word = |half: &[u8]| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(half);
            u64::from_le_bytes(bytes)
        };
        (word(&cell[..8]), word(&cell[8..]))
    }

    fn cell_of(low: u64, high: u64) -> [u8; 16] {
        let mut cell = [0u8; 16];
        cell[..8].copy_from_slice(&low.to_le_bytes());
        cell[8..].copy_from_slice(&high.to_le_bytes());
        cell
    }

    /// `id`'s word as `blaze_engine::CellId`'s is to be: the subject's, then the kind, the
    /// cell's low word and its high word folded in, in that order; or a plant that leaves one
    /// out.
    fn id_word(id: &Id, plant: WordPlant) -> u64 {
        let (low, high) = halves(&id.cell);
        let mut word = id.subject.digest_word();
        if plant != WordPlant::DropsKind {
            word = fold_part(word, u64::from(id.kind));
        }
        word = fold_part(word, low);
        if plant != WordPlant::DropsHigh {
            word = fold_part(word, high);
        }
        word
    }

    /// Ids alike in all but some fields, each a shape a table of engine cells holds.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Family {
        /// One task's cells, numbered in the cell's low word: one subject, one kind.
        Low,
        /// One task's cells, numbered in the cell's high word.
        High,
        /// One cell under many subjects.
        Subjects,
        /// One subject's cells under every kind.
        Kinds,
    }

    const FAMILIES: [Family; 4] = [Family::Low, Family::High, Family::Subjects, Family::Kinds];

    /// The ids of a family, all distinct.
    const IDS: u64 = 1 << 15;
    /// The index buckets the spread is counted in: a table of `IDS` keys places them by the low
    /// twelve bits at a mean of eight a bucket, and tags them by the top seven at a mean of 256.
    const BUCKETS: usize = 1 << 12;
    /// Four times the mean: a uniform hash's fullest of 4096 buckets is about 20.
    const MOST_IN_BUCKET: usize = 32;
    /// Twice the mean: a uniform hash's fullest of 128 tags is about 300.
    const MOST_IN_TAG: usize = 512;

    /// The `at`th id of `family`, its digests from the stream `seed`.
    fn family_id(family: Family, seed: u64, at: u64) -> Id {
        let subject = digest_of(seed, 0);
        match family {
            Family::Low => Id { kind: 1, subject, cell: cell_of(at, 0) },
            Family::High => Id { kind: 1, subject, cell: cell_of(0, at) },
            Family::Subjects => Id { kind: 1, subject: digest_of(seed, at), cell: cell_of(7, 0) },
            Family::Kinds => Id {
                kind: u8::try_from(at % 256).expect("a remainder of 256 fits a byte"),
                subject,
                cell: cell_of(at / 256, 0),
            },
        }
    }

    /// What the spread oracle saw.
    #[derive(Debug)]
    enum WordBroken {
        /// The fullest of the index buckets held `most` of the family's ids.
        Crowded { family: Family, most: usize },
        /// The fullest of the 128 tags held `most` of them.
        Tagged { family: Family, most: usize },
    }

    impl fmt::Display for WordBroken {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                WordBroken::Crowded { family, most } => write!(
                    f,
                    "{family:?}: {most} of {IDS} ids in one of {BUCKETS} buckets (at most {MOST_IN_BUCKET})"
                ),
                WordBroken::Tagged { family, most } => write!(
                    f,
                    "{family:?}: {most} of {IDS} ids under one of 128 tags (at most {MOST_IN_TAG})"
                ),
            }
        }
    }

    blaze_sim::plants! {
        /// Words that leave a field of an id out, each caught by the family that differs only
        /// there.
        enum WordPlant breaks WordBroken {
            /// The word as it is to be.
            None,
            /// The kind is not folded in: one subject's cells under many kinds share words.
            DropsKind => WordBroken::Crowded { family: Family::Kinds, .. },
            /// The cell's high word is not folded in: cells numbered there share one word.
            DropsHigh => WordBroken::Crowded { family: Family::High, .. },
        }
    }

    /// The ids of a drawn family spread over the buckets and the tags as a uniform hash's do.
    fn spread(tape: &Tape, plant: WordPlant) -> Result<(), Broken<WordBroken>> {
        let family = *tape.one_of(&FAMILIES, "family");
        let seed = tape.int::<u64>("seed");
        let mut buckets = [0usize; BUCKETS];
        let mut tags = [0usize; 128];
        for at in 0..IDS {
            let word = id_word(&family_id(family, seed, at), plant);
            buckets[(word & (BUCKETS as u64 - 1)) as usize] += 1;
            tags[(word >> 57) as usize] += 1;
        }
        let most = buckets.iter().copied().max().unwrap_or(0);
        if most > MOST_IN_BUCKET {
            return Err(Broken::Oracle(WordBroken::Crowded { family, most }));
        }
        let most = tags.iter().copied().max().unwrap_or(0);
        if most > MOST_IN_TAG {
            return Err(Broken::Oracle(WordBroken::Tagged { family, most }));
        }
        Ok(())
    }

    /// The word as it is to be spreads every family, and each plant that drops a field is
    /// caught by the family that differs only there (rule 53).
    #[test]
    fn a_digest_keys_word_spreads_every_family() {
        controls(Config::cases(64), spread);
    }

    /// A map and an insertion-ordered map on `DigestState` hold what a model holds, through
    /// drawn inserts, lookups and removals of ids from small pools: one subject's many cells,
    /// one cell under many subjects, kinds apart, and repeats.
    #[test]
    fn tables_on_digest_state_hold_what_a_model_holds() {
        check(Config::cases(300), |tape: &Tape| {
            let seed = tape.int::<u64>("seed");
            let ops = tape.vec(0..=200, "ops", |tape| {
                let id = Id {
                    kind: tape.int_in(0..=2u8, "kind"),
                    subject: digest_of(seed, tape.int_in(0..=2u64, "subject")),
                    cell: cell_of(tape.int_in(0..=3u64, "cell low"), tape.int_in(0..=3u64, "cell high")),
                };
                (tape.index(3, "op"), id, tape.int::<u32>("value"))
            });
            let mut map: Map<Id, u32, DigestState, Alloc<'static>> =
                Map::new(MapOpts { hasher: DigestState, alloc: heap(), capacity: None });
            let mut order: OrderMap<Id, u32, DigestState, Alloc<'static>> =
                OrderMap::new(OrderMapOpts { hasher: DigestState, alloc: heap(), capacity: None });
            let mut model: BTreeMap<Id, u32> = BTreeMap::new();
            let mut in_order: Vec<(Id, u32)> = Vec::new();
            for (op, id, value) in ops {
                let held = in_order.iter().position(|(key, _)| *key == id);
                match op {
                    0 => {
                        assert_eq!(map.insert(id, value), model.insert(id, value));
                        let old = match held {
                            Some(at) => Some(core::mem::replace(&mut in_order[at].1, value)),
                            None => {
                                in_order.push((id, value));
                                None
                            }
                        };
                        assert_eq!(order.insert(id, value), old);
                    }
                    1 => {
                        assert_eq!(map.remove(&id), model.remove(&id));
                        assert_eq!(order.shift_remove(&id), held.map(|at| in_order.remove(at).1));
                    }
                    _ => {
                        assert_eq!(map.get(&id), model.get(&id));
                        assert_eq!(order.get(&id), held.map(|at| &in_order[at].1));
                    }
                }
            }
            assert_eq!(map.len(), model.len());
            assert!(order.iter().map(|(key, value)| (*key, *value)).eq(in_order.iter().copied()));
        });
    }
}
