//! SipHash-1-3 (Aumasson and Bernstein, 2012; the variant std's maps use) of a byte string
//! under a 128-bit key: the hash for a table whose keys an attacker may pick (identifiers,
//! literals, a stored tree's text), where a fixed hash lets them pick many keys with one hash
//! and turn every insert into a walk of one chain (HashDoS).
//!
//! The other hashers of this crate have fixed seeds so that a run is reproducible, and every
//! step of a multiply-rotate hash such as `blaze_phf::name_hash` inverts, so full 64-bit
//! collisions of it cost a few thousand tries each. SipHash is a keyed PRF: without the key a
//! hash says nothing about another's, and a key drawn at random ([`SipKey::random`]) is never
//! shown. Where a table is persisted its key must be a pure function of its content (AGENTS.md
//! rule 25): derive it from the values with BLAKE3, so the keys can't be picked before the key
//! is known.
//!
//! The algorithm is fixed here, never std's (whose `DefaultHasher` may change), so a key and
//! bytes give the same hash in every build: an image's index may be laid out by it. Pinned by
//! std's own SipHash (2-4, and 1-3 behind `hashmap_internals`) and the reference vectors in
//! this crate's tests.
//!
//! A value that is not a byte string (a type's kind, a path of names, a debugger's type) is
//! keyed through its `Hash` by [`SipHasher`] ([`SipKey::hash_one`], or a table's
//! `BuildHasher`): the same SipHash-1-3 of the bytes its `Hash` writes, one after another,
//! integers little-endian at a fixed width. A table in a `static` takes its key from a
//! [`LazyKey`], drawn the first time it is used.
//!
//! Cost: one round of four adds, rotates and xors for every 8 bytes, then 4 rounds: about
//! 15 ns for a 16-byte name, where foldhash's fixed hasher takes 3.

use std::{
    hash::{BuildHasher, Hash, Hasher},
    sync::OnceLock,
};

/// A SipHash key: what [`SipKey::hash`] is keyed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SipKey {
    k0: u64,
    k1: u64,
}

impl SipKey {
    pub const fn new(k0: u64, k1: u64) -> SipKey {
        SipKey { k0, k1 }
    }

    /// A key from the operating system's randomness, unknown outside this process: for a
    /// table nothing persists the layout of. Two outputs of std's `RandomState`, whose keys
    /// std draws from the OS (`getrandom`, `arc4random`, WASI's `random_get`) once per thread
    /// and steps for each state, so no two calls give related keys.
    pub fn random() -> SipKey {
        #[allow(
            clippy::disallowed_types,
            reason = "std's one door to the OS's randomness: used only to draw a SipHash key, never as a map's hasher"
        )]
        let state = std::hash::RandomState::new();
        SipKey::new(state.hash_one(0x5349_5030_u64), state.hash_one(0x5349_5031_u64))
    }

    /// The key's two words, for a format that stores it.
    pub const fn words(self) -> [u64; 2] {
        [self.k0, self.k1]
    }

    /// SipHash-1-3 of `bytes` under this key.
    #[inline]
    pub fn hash(self, bytes: &[u8]) -> u64 {
        sip::<1, 3>(self, bytes)
    }

    /// A [`SipHasher`] under this key, with nothing written.
    #[inline]
    pub fn hasher(self) -> SipHasher {
        SipHasher { v: init(self), tail: 0, ntail: 0, length: 0 }
    }

    /// SipHash-1-3 under this key of the bytes `value`'s `Hash` writes.
    #[inline]
    pub fn hash_one<T: Hash + ?Sized>(self, value: &T) -> u64 {
        let mut hasher = self.hasher();
        value.hash(&mut hasher);
        hasher.finish()
    }
}

/// A table keyed by a [`SipKey`] hashes each value with its [`SipHasher`].
impl BuildHasher for SipKey {
    type Hasher = SipHasher;

    #[inline]
    fn build_hasher(&self) -> SipHasher {
        self.hasher()
    }
}

/// A table's [`SipKey`], drawn by [`SipKey::random`] the first time it is asked for, then the
/// same for the table's life: what a table that lives in a `static`, made by a `const fn`
/// before any code runs, is keyed by. Asking is one acquire load once the key is drawn.
#[derive(Default)]
pub struct LazyKey(OnceLock<SipKey>);

impl LazyKey {
    pub const fn new() -> LazyKey {
        LazyKey(OnceLock::new())
    }

    /// The key, drawn now if this is the first ask.
    #[inline]
    pub fn get(&self) -> SipKey {
        *self.0.get_or_init(SipKey::random)
    }
}

/// SipHash-1-3, under one key, of every byte written to it so far: [`SipKey::hash`] of the
/// bytes of every `write` one after another, each integer written as its little-endian bytes
/// at its own width (`usize` and `isize` as 64 bits, so a hash is the same on every target).
/// [`Hasher::finish`] reads without consuming, so one pass over a path can hash each of its
/// prefixes.
#[derive(Clone, Debug)]
pub struct SipHasher {
    v: [u64; 4],
    /// The bytes written since the last full word, little-endian in the low `ntail` bytes.
    tail: u64,
    ntail: usize,
    /// Every byte written; its low byte goes into the last word.
    length: usize,
}

impl SipHasher {
    /// Writes the low `size` bytes (1 to 8) of `word`, whose other bytes are 0.
    #[inline(always)]
    fn short_write(&mut self, word: u64, size: usize) {
        debug_assert!((1..=8).contains(&size) && (size == 8 || word >> (8 * size) == 0));
        self.length += size;
        self.tail |= word << (8 * self.ntail);
        let needed = 8 - self.ntail;
        if size < needed {
            self.ntail += size;
            return;
        }
        compress::<1>(&mut self.v, self.tail);
        self.ntail = size - needed;
        self.tail = if needed < 8 { word >> (8 * needed) } else { 0 };
    }
}

impl Hasher for SipHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.length += bytes.len();
        let mut rest = bytes;
        if self.ntail != 0 {
            let fill = (8 - self.ntail).min(rest.len());
            self.tail |= load(&rest[..fill]) << (8 * self.ntail);
            if self.ntail + fill < 8 {
                self.ntail += fill;
                return;
            }
            compress::<1>(&mut self.v, self.tail);
            rest = &rest[fill..];
        }
        while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
            compress::<1>(&mut self.v, u64::from_le_bytes(*chunk));
            rest = tail;
        }
        self.tail = load(rest);
        self.ntail = rest.len();
    }

    #[inline]
    fn finish(&self) -> u64 {
        finish::<1, 3>(self.v, ((self.length as u64) << 56) | self.tail)
    }

    #[inline]
    fn write_u8(&mut self, it: u8) {
        self.short_write(u64::from(it), 1);
    }

    #[inline]
    fn write_u16(&mut self, it: u16) {
        self.short_write(u64::from(it), 2);
    }

    #[inline]
    fn write_u32(&mut self, it: u32) {
        self.short_write(u64::from(it), 4);
    }

    #[inline]
    fn write_u64(&mut self, it: u64) {
        self.short_write(it, 8);
    }

    #[inline]
    fn write_u128(&mut self, it: u128) {
        self.short_write(it as u64, 8);
        self.short_write((it >> 64) as u64, 8);
    }

    #[inline]
    fn write_usize(&mut self, it: usize) {
        self.short_write(it as u64, 8);
    }

    #[inline]
    fn write_i8(&mut self, it: i8) {
        self.write_u8(it as u8);
    }

    #[inline]
    fn write_i16(&mut self, it: i16) {
        self.write_u16(it as u16);
    }

    #[inline]
    fn write_i32(&mut self, it: i32) {
        self.write_u32(it as u32);
    }

    #[inline]
    fn write_i64(&mut self, it: i64) {
        self.write_u64(it as u64);
    }

    #[inline]
    fn write_i128(&mut self, it: i128) {
        self.write_u128(it as u128);
    }

    #[inline]
    fn write_isize(&mut self, it: isize) {
        self.write_u64(it as u64);
    }
}

/// Up to 8 bytes as the low bytes of a little-endian word.
#[inline(always)]
fn load(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() <= 8);
    let mut word = [0; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

#[inline(always)]
fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

/// The state SipHash starts from under `key`.
#[inline(always)]
fn init(key: SipKey) -> [u64; 4] {
    [
        key.k0 ^ 0x736f_6d65_7073_6575,
        key.k1 ^ 0x646f_7261_6e64_6f6d,
        key.k0 ^ 0x6c79_6765_6e65_7261,
        key.k1 ^ 0x7465_6462_7974_6573,
    ]
}

/// Takes in one word: `C` rounds.
#[inline(always)]
fn compress<const C: usize>(v: &mut [u64; 4], m: u64) {
    v[3] ^= m;
    (0..C).for_each(|_| round(v));
    v[0] ^= m;
}

/// Takes in the last word (the bytes left, little-endian, and the length's low byte on top)
/// with `C` rounds, then `D` rounds.
#[inline(always)]
fn finish<const C: usize, const D: usize>(mut v: [u64; 4], last: u64) -> u64 {
    compress::<C>(&mut v, last);
    v[2] ^= 0xff;
    (0..D).for_each(|_| round(&mut v));
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// SipHash-`C`-`D`: `C` rounds a word, `D` to finish.
#[inline]
pub(crate) fn sip<const C: usize, const D: usize>(key: SipKey, bytes: &[u8]) -> u64 {
    let mut v = init(key);
    let mut rest = bytes;
    while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
        compress::<C>(&mut v, u64::from_le_bytes(*chunk));
        rest = tail;
    }
    finish::<C, D>(v, ((bytes.len() as u64) << 56) | load(rest))
}
