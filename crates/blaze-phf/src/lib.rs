//! Perfect hashes of fixed name sets (keywords, lint names, attribute names), built by `const
//! fn` as the crate that declares the set compiles: no build step to rerun, no table built at
//! run time, no dependency. A set that names a key twice, or that no seed hashes perfectly,
//! fails to compile.
//!
//! The scheme is PTHash's (Pibiri and Trani, SIGIR 2021): a key's 64-bit hash picks a bucket,
//! each bucket has a pilot found when the table is built, and the hash mixed with its bucket's
//! pilot picks the key's slot. Buckets are placed largest first. With at most half the slots
//! taken and about four keys a bucket, a pilot is found in a few tries, so a set of a thousand
//! names costs const evaluation a few hundred thousand steps.
//!
//! Cost: 2 bytes a slot (two slots a key, rounded up to a power of two) and 2 bytes a bucket (a
//! quarter of a bucket a key): about 4.5 to 8.5 bytes a key. A lookup is one hash of the bytes
//! (a 64-bit multiply for every 8 of them and two more), two dependent loads and one compare of
//! the one key it can be.
//!
//! ```
//! const KEYWORDS: &[&str] = &["fn", "let", "match"];
//! blaze_phf::phf_table!(static TABLE = KEYWORDS;);
//! assert_eq!(TABLE.get("let"), Some(1));
//! assert_eq!(TABLE.get("lot"), None);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

const K0: u64 = 0x243f_6a88_85a3_08d3;
const K1: u64 = 0x1319_8a2e_0370_7344;
const K2: u64 = 0xa409_3822_299f_31d0;

/// A slot no key is in.
const EMPTY: u16 = u16::MAX;
/// The pilots tried for one bucket before the next seed is.
const PILOTS: u16 = 4096;
/// The seeds tried before the set is refused.
const SEEDS: u64 = 64;
/// The largest bucket placed; a seed that makes a larger one is passed over.
const LARGEST: usize = 32;

/// The slots of a table of `keys` keys: twice as many, rounded up to a power of two.
pub const fn slots(keys: usize) -> usize {
    (2 * keys).next_power_of_two()
}

/// The buckets of a table of `keys` keys: one for every four.
pub const fn buckets(keys: usize) -> usize {
    keys / 4 + 1
}

#[inline(always)]
const fn fold(a: u64, b: u64) -> u64 {
    let wide = (a as u128) * (b as u128);
    (wide as u64) ^ ((wide >> 64) as u64)
}

/// [`name_hash`]'s multiplier.
pub const NAME_K: u64 = 0xf135_7aea_2e62_a9c5;

/// [`name_hash`]'s state before the first word: the length.
#[inline(always)]
pub const fn name_hash_start(len: usize) -> u64 {
    (len as u64).wrapping_mul(NAME_K)
}

/// [`name_hash`]'s state after one more 8-byte word, little-endian (the last zero above the
/// name's end).
#[inline(always)]
pub const fn name_hash_word(h: u64, word: u64) -> u64 {
    (h ^ word).wrapping_mul(NAME_K).rotate_left(26)
}

/// [`name_hash`] from its state after the last word: murmur3's 64-bit finalizer, so the top
/// bits and the low ones are both well mixed.
#[inline(always)]
pub const fn name_hash_finish(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// The hash `intern`'s tables key a value's bytes by, and a [`Phf`] made with
/// [`name_phf_table!`](crate::name_phf_table) its keys by: FxHash's multiply-rotate over 8-byte
/// words, the length first, then [`name_hash_finish`]. An image's symbol index is laid out by
/// it, so it never changes. It is here, where both reach it, so that one hash of a name finds it
/// in a compiled-in set and, if it is not there, in the interner's table. A name of 1 to 8
/// bytes is one word: `name_hash_finish(name_hash_word(name_hash_start(len), word))`.
#[inline]
pub const fn name_hash(bytes: &[u8]) -> u64 {
    let mut h = name_hash_start(bytes.len());
    let mut rest = bytes;
    while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
        h = name_hash_word(h, u64::from_le_bytes(*chunk));
        rest = tail;
    }
    if !rest.is_empty() {
        let mut last = 0u64;
        let mut i = 0;
        while i < rest.len() {
            last |= (rest[i] as u64) << (8 * i);
            i += 1;
        }
        h = name_hash_word(h, last);
    }
    name_hash_finish(h)
}

/// The hash of `bytes` under `seed`: the same at compile time and at run time.
#[inline]
pub const fn hash(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed ^ K0;
    let mut rest = bytes;
    while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
        h = fold(h ^ u64::from_le_bytes(*chunk), K1);
        rest = tail;
    }
    let mut last = 0u64;
    let mut i = 0;
    while i < rest.len() {
        last |= (rest[i] as u64) << (8 * i);
        i += 1;
    }
    fold(fold(h ^ last, K1) ^ bytes.len() as u64, K2)
}

#[inline(always)]
const fn bucket(h: u64, buckets: usize) -> usize {
    (((h >> 32) * buckets as u64) >> 32) as usize
}

#[inline(always)]
const fn slot(h: u64, pilot: u16, slots: usize) -> usize {
    fold(h ^ pilot as u64, K2) as usize & (slots - 1)
}

/// `a == b`, callable in a `const`, where slices' `==` is not.
pub const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// A perfect hash of the fixed set `keys`: each key's index in `keys` from its bytes. Declare
/// one with [`phf_table!`](crate::phf_table), which sizes it, or with
/// [`name_phf_table!`](crate::name_phf_table) for one keyed by [`name_hash`] (`BY_NAME`), which a
/// caller that already has a name's `name_hash` looks up with no second hash of its bytes
/// ([`Phf::index_by_name_hash`]).
pub struct Phf<const SLOTS: usize, const BUCKETS: usize, const BY_NAME: bool = false> {
    keys: &'static [&'static str],
    seed: u64,
    pilots: [u16; BUCKETS],
    /// The index in `keys` of the key in each slot, or [`EMPTY`].
    slots: [u16; SLOTS],
}

/// A [`name_hash`] mixed with a table's seed: a bijection of the hash for each seed, so two
/// keys of one `name_hash` are in one slot under every seed, and the set is refused.
#[inline(always)]
const fn seeded(name_hash: u64, seed: u64) -> u64 {
    fold(name_hash ^ seed ^ K0, K1)
}

impl<const SLOTS: usize, const BUCKETS: usize, const BY_NAME: bool> Phf<SLOTS, BUCKETS, BY_NAME> {
    /// The hash a key's bucket and slot are found from.
    #[inline(always)]
    const fn key_hash(bytes: &[u8], seed: u64) -> u64 {
        match BY_NAME {
            true => seeded(name_hash(bytes), seed),
            false => hash(bytes, seed),
        }
    }

    /// The table of `keys`. Panics, which in a `static` is a compile error, when a key is
    /// named twice or no seed of the first 64 gives a perfect hash.
    pub const fn new(keys: &'static [&'static str]) -> Self {
        assert!(SLOTS.is_power_of_two(), "a power of two of slots");
        assert!(keys.len() <= SLOTS / 2 || keys.is_empty(), "at most half the slots taken");
        assert!(keys.len() < EMPTY as usize, "fewer than 65535 keys");
        assert!(BUCKETS > 0 && BUCKETS <= u16::MAX as usize, "between 1 and 65535 buckets");
        let mut seed = 0;
        while seed < SEEDS {
            if let Some(table) = Self::with_seed(keys, seed) {
                return table;
            }
            seed += 1;
        }
        panic!("no perfect hash of these keys in 64 seeds")
    }

    const fn with_seed(keys: &'static [&'static str], seed: u64) -> Option<Self> {
        let n = keys.len();
        let mut hashes = [0u64; SLOTS];
        let mut sizes = [0u16; BUCKETS];
        let mut i = 0;
        while i < n {
            let h = Self::key_hash(keys[i].as_bytes(), seed);
            hashes[i] = h;
            sizes[bucket(h, BUCKETS)] += 1;
            i += 1;
        }
        // The keys of bucket `b` are `members[starts[b]..starts[b] + sizes[b]]`.
        let mut starts = [0u16; BUCKETS];
        let mut largest = 0;
        let mut b = 0;
        let mut at = 0u16;
        while b < BUCKETS {
            starts[b] = at;
            at += sizes[b];
            if sizes[b] as usize > largest {
                largest = sizes[b] as usize;
            }
            b += 1;
        }
        if largest > LARGEST {
            return None;
        }
        let mut members = [0u16; SLOTS];
        let mut filled = [0u16; BUCKETS];
        i = 0;
        while i < n {
            let b = bucket(hashes[i], BUCKETS);
            members[(starts[b] + filled[b]) as usize] = i as u16;
            filled[b] += 1;
            i += 1;
        }
        // Two keys of one hash are in one bucket, and no pilot parts them.
        b = 0;
        while b < BUCKETS {
            let (start, size) = (starts[b] as usize, sizes[b] as usize);
            let mut x = 0;
            while x < size {
                let mut y = x + 1;
                while y < size {
                    let (p, q) = (members[start + x] as usize, members[start + y] as usize);
                    if hashes[p] == hashes[q] {
                        if bytes_eq(keys[p].as_bytes(), keys[q].as_bytes()) {
                            panic!("a key of the perfect hash is named twice");
                        }
                        return None;
                    }
                    y += 1;
                }
                x += 1;
            }
            b += 1;
        }
        let mut slots = [EMPTY; SLOTS];
        let mut pilots = [0u16; BUCKETS];
        let mut size = largest;
        while size > 0 {
            b = 0;
            while b < BUCKETS {
                if sizes[b] as usize == size {
                    let start = starts[b] as usize;
                    let mut pilot = 0u16;
                    let mut placed = false;
                    while !placed {
                        if pilot == PILOTS {
                            return None;
                        }
                        let mut taken = [0usize; LARGEST];
                        let mut free = true;
                        let mut x = 0;
                        while free && x < size {
                            let s = slot(hashes[members[start + x] as usize], pilot, SLOTS);
                            free = slots[s] == EMPTY;
                            let mut y = 0;
                            while free && y < x {
                                free = taken[y] != s;
                                y += 1;
                            }
                            taken[x] = s;
                            x += 1;
                        }
                        if free {
                            x = 0;
                            while x < size {
                                slots[taken[x]] = members[start + x];
                                x += 1;
                            }
                            pilots[b] = pilot;
                            placed = true;
                        } else {
                            pilot += 1;
                        }
                    }
                }
                b += 1;
            }
            size -= 1;
        }
        Some(Phf { keys, seed, pilots, slots })
    }

    #[inline(always)]
    const fn candidate(&self, bytes: &[u8]) -> usize {
        self.candidate_of(Self::key_hash(bytes, self.seed))
    }

    #[inline(always)]
    const fn candidate_of(&self, h: u64) -> usize {
        self.slots[slot(h, self.pilots[bucket(h, BUCKETS)], SLOTS)] as usize
    }

    /// [`Phf::index`] of `bytes` whose [`name_hash`] is `name_hash`, for a table keyed by it:
    /// the bytes are read only to compare them with the one key they can be.
    #[inline]
    pub fn index_by_name_hash(&self, name_hash: u64, bytes: &[u8]) -> Option<usize> {
        const { assert!(BY_NAME, "a table keyed by name_hash (name_phf_table!)") };
        let at = self.candidate_of(seeded(name_hash, self.seed));
        let key = self.keys.get(at)?;
        (key.as_bytes() == bytes).then_some(at)
    }

    /// The index in the keys of `bytes`, if it is one.
    #[inline]
    pub fn index(&self, bytes: &[u8]) -> Option<usize> {
        let at = self.candidate(bytes);
        let key = self.keys.get(at)?;
        (key.as_bytes() == bytes).then_some(at)
    }

    /// The index in the keys of `name`, if it is one.
    #[inline]
    pub fn get(&self, name: &str) -> Option<usize> {
        self.index(name.as_bytes())
    }

    /// [`Phf::index`] in a `const`: for building other tables from this one.
    pub const fn index_const(&self, bytes: &[u8]) -> Option<usize> {
        let at = self.candidate(bytes);
        if at < self.keys.len() && bytes_eq(self.keys[at].as_bytes(), bytes) {
            Some(at)
        } else {
            None
        }
    }

    /// The keys, in the order of their indices.
    pub const fn keys(&self) -> &'static [&'static str] {
        self.keys
    }
}

/// Declares `static $name`, the [`Phf`] of the keys `$keys`, sized for them: a `const` (or
/// constant expression) of type `&'static [&'static str]`.
#[macro_export]
macro_rules! phf_table {
    ($(#[$attr:meta])* $vis:vis static $name:ident = $keys:expr $(;)?) => {
        $(#[$attr])*
        $vis static $name: $crate::Phf<
            { $crate::slots($keys.len()) },
            { $crate::buckets($keys.len()) },
        > = $crate::Phf::new($keys);
    };
}

/// [`phf_table!`] keyed by [`name_hash`]: also looked up by a hash the caller already has
/// ([`Phf::index_by_name_hash`]).
#[macro_export]
macro_rules! name_phf_table {
    ($(#[$attr:meta])* $vis:vis static $name:ident = $keys:expr $(;)?) => {
        $(#[$attr])*
        $vis static $name: $crate::Phf<
            { $crate::slots($keys.len()) },
            { $crate::buckets($keys.len()) },
            true,
        > = $crate::Phf::new($keys);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
        "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "macro", "match",
        "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "try", "type", "unsafe", "use", "where", "while", "yield", "gen",
    ];
    crate::phf_table!(static TABLE = KEYWORDS;);
    const EMPTY_KEYS: &[&str] = &[];
    crate::phf_table!(static NONE = EMPTY_KEYS;);
    const ONE_KEY: &[&str] = &[""];
    crate::phf_table!(static ONE = ONE_KEY;);

    /// Every key lands on its own slot, and nothing else is found.
    fn check<const S: usize, const B: usize>(table: &Phf<S, B>, others: &[&str]) {
        let mut seen = vec![false; S];
        for (at, key) in table.keys().iter().enumerate() {
            assert_eq!(table.get(key), Some(at), "{key:?}");
            assert_eq!(table.index_const(key.as_bytes()), Some(at), "{key:?}");
            let slot = table.slots.iter().position(|&it| it as usize == at).expect("placed");
            assert!(!std::mem::replace(&mut seen[slot], true), "two keys in slot {slot}");
        }
        assert_eq!(table.slots.iter().filter(|&&it| it != EMPTY).count(), table.keys().len());
        for other in others {
            let linear = table.keys().iter().position(|it| it == other);
            assert_eq!(table.get(other), linear, "{other:?}");
            assert_eq!(table.index_const(other.as_bytes()), linear, "{other:?}");
        }
    }

    #[test]
    fn keywords() {
        check(&TABLE, &["", "f", "fnn", "Fn", "self_", "selfSelf", "unsafe ", "whil", "asyncawait"]);
        assert_eq!(TABLE.get("Self"), Some(30));
        check(&NONE, &["", "a"]);
        check(&ONE, &["", "a", "\0"]);
    }

    #[test]
    fn many_names() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x6e61_6d65_73);
        let alphabet = b"abcdefghijklmnopqrstuvwxyz_0123456789";
        let mut names: Vec<String> = Vec::new();
        let mut unique = std::collections::HashSet::new();
        while names.len() < 3000 {
            let len = 1 + rng.index(40);
            let name: String =
                (0..len).map(|_| alphabet[rng.index(alphabet.len())] as char).collect();
            if unique.insert(name.clone()) {
                names.push(name);
            }
        }
        let keys: &'static [&'static str] =
            Box::leak(names.iter().map(|it| &*Box::leak(it.clone().into_boxed_str())).collect());
        let table: Phf<{ slots(3000) }, { buckets(3000) }> = Phf::new(keys);
        let mut others: Vec<String> = names.iter().map(|it| format!("{it}_")).collect();
        others.extend(names.iter().map(|it| it[..it.len() - 1].to_owned()));
        others.extend(names.iter().map(|it| it.to_uppercase()));
        let others: Vec<&str> = others.iter().map(String::as_str).collect();
        check(&table, &others);
    }

    #[test]
    #[should_panic(expected = "named twice")]
    fn a_key_named_twice_is_refused() {
        let _: Phf<{ slots(3) }, { buckets(3) }> = Phf::new(&["a", "b", "a"]);
    }

    #[test]
    fn hash_reads_every_byte_and_the_length() {
        let base = hash(b"abcdefghijklmnop", 0);
        for i in 0..16 {
            let mut bytes = *b"abcdefghijklmnop";
            bytes[i] ^= 1;
            assert_ne!(hash(&bytes, 0), base, "byte {i}");
        }
        assert_ne!(hash(b"a", 0), hash(b"a\0", 0));
        assert_ne!(hash(b"", 0), hash(b"\0", 0));
        assert_ne!(hash(b"abc", 0), hash(b"abc", 1));
    }

    const _: () = assert!(bytes_eq(b"lint", b"lint") && !bytes_eq(b"lint", b"lints") && bytes_eq(b"", b""));

    crate::name_phf_table!(static BY_NAME = KEYWORDS;);

    /// `intern`'s `value_hash` as it was written before it moved here, closures and all: the
    /// hash images' symbol indices are laid out by.
    fn value_hash_as_written(bytes: &[u8]) -> u64 {
        const K: u64 = 0xf135_7aea_2e62_a9c5;
        let step = |h: u64, word: u64| (h ^ word).wrapping_mul(K).rotate_left(26);
        let mut h = (bytes.len() as u64).wrapping_mul(K);
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            h = step(h, u64::from_le_bytes(word.try_into().expect("8 bytes")));
        }
        let rest = words.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            h = step(h, u64::from_le_bytes(last));
        }
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }

    #[test]
    fn name_hash_is_the_interners() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x6e61_6d65_68);
        for len in 0..80 {
            for _ in 0..50 {
                let bytes: Vec<u8> = (0..len).map(|_| rng.index(256) as u8).collect();
                assert_eq!(name_hash(&bytes), value_hash_as_written(&bytes), "{bytes:?}");
            }
        }
        for name in ["", "a", "self", "abcdefgh", "abcdefghi", "ünïcödé"] {
            assert_eq!(name_hash(name.as_bytes()), value_hash_as_written(name.as_bytes()));
        }
        // A name of one word is one step from the length.
        for name in ["a", "Ok", "self", "Some", "iter_mut", "abcdefgh"] {
            let mut word = [0u8; 8];
            word[..name.len()].copy_from_slice(name.as_bytes());
            let one = name_hash_finish(name_hash_word(name_hash_start(name.len()), u64::from_le_bytes(word)));
            assert_eq!(one, name_hash(name.as_bytes()), "{name}");
        }
        const AT_COMPILE_TIME: u64 = name_hash(b"iter");
        assert_eq!(AT_COMPILE_TIME, value_hash_as_written(b"iter"));
    }

    #[test]
    fn a_table_by_name_hash_finds_by_either() {
        for (at, key) in KEYWORDS.iter().enumerate() {
            assert_eq!(BY_NAME.get(key), Some(at), "{key:?}");
            assert_eq!(BY_NAME.index_const(key.as_bytes()), Some(at), "{key:?}");
            assert_eq!(BY_NAME.index_by_name_hash(name_hash(key.as_bytes()), key.as_bytes()), Some(at));
        }
        for other in ["", "f", "fnn", "Fn", "self_", "selfSelf", "unsafe ", "whil", "asyncawait"] {
            assert_eq!(BY_NAME.get(other), None, "{other:?}");
            assert_eq!(BY_NAME.index_by_name_hash(name_hash(other.as_bytes()), other.as_bytes()), None);
        }
        // A hash given for other bytes finds nothing: the key is compared.
        assert_eq!(BY_NAME.index_by_name_hash(name_hash(b"fn"), b"let"), None);
        let mut seen = vec![false; KEYWORDS.len()];
        for at in BY_NAME.slots.iter().filter(|&&it| it != EMPTY) {
            assert!(!std::mem::replace(&mut seen[*at as usize], true), "key {at} placed twice");
        }
        assert!(seen.iter().all(|&it| it), "every key placed");
    }

    #[hegel::test]
    fn bytes_eq_is_slice_eq(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let a: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(3)).max_size(7));
        let b: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(3)).max_size(7));
        assert_eq!(bytes_eq(&a, &b), a == b);
        assert!(bytes_eq(&a, &a));
    }
}
