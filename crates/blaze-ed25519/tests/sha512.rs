//! SHA-512 against FIPS 180-4's examples, and against a second implementation written here to be
//! obviously right rather than fast: one shot over a padded copy, the message schedule as the
//! standard's 80-word array, and the round constants and initial value derived from the first
//! 80 primes instead of typed in.
//!
//! The expected digests were checked in the session that wrote them against the system's
//! `shasum -a 512` on the same inputs.

use blaze_ed25519::sha512::{self, IV, K, Sha512};
use blaze_sim::Config;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex")).collect()
}

const EMPTY: &str = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
const ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
/// FIPS 180-4's two-block example, 112 bytes.
const TWO_BLOCK_INPUT: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
const TWO_BLOCK: &str = "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909";
const MILLION_A: &str = "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b";

#[test]
fn the_standards_examples() {
    assert_eq!(sha512::hash(b"").to_vec(), hex(EMPTY));
    assert_eq!(sha512::hash(b"abc").to_vec(), hex(ABC));
    assert_eq!(sha512::hash(TWO_BLOCK_INPUT).to_vec(), hex(TWO_BLOCK));
}

/// A million 'a's, fed a thousand at a time (no buffer of a million bytes); under Miri, which
/// would take an hour on it, skipped.
#[test]
#[cfg_attr(miri, ignore = "a million bytes is hours under Miri")]
fn a_million_a() {
    let mut hasher = Sha512::new();
    let thousand = [b'a'; 1000];
    for _ in 0..1000 {
        hasher.update(&thousand);
    }
    assert_eq!(hasher.finalize().to_vec(), hex(MILLION_A));
}

/// `floor(x^(1/n))` for x below 2^256, n of 2 or 3, by bisection on exact integers.
fn integer_root(x: &Big, n: u32) -> u128 {
    let (mut lo, mut hi) = (0u128, 1u128 << 70);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let mut power = Big::from(1);
        for _ in 0..n {
            power = power.mul(mid);
        }
        if power.le(x) { lo = mid } else { hi = mid - 1 }
    }
    lo
}

/// A 256-bit unsigned integer, little-endian limbs: just enough for the roots above.
#[derive(Clone, Copy)]
struct Big([u64; 4]);

impl Big {
    fn from(x: u128) -> Big {
        Big([x as u64, (x >> 64) as u64, 0, 0])
    }

    fn shifted(x: u64, by: u32) -> Big {
        let mut out = [0u64; 4];
        let (limb, bit) = ((by / 64) as usize, by % 64);
        out[limb] = x << bit;
        if bit > 0 && limb + 1 < 4 {
            out[limb + 1] = x >> (64 - bit);
        }
        Big(out)
    }

    /// `self * m`; panics past 256 bits.
    fn mul(self, m: u128) -> Big {
        let m = [m as u64, (m >> 64) as u64];
        let mut out = [0u128; 6];
        for (i, &a) in self.0.iter().enumerate() {
            for (j, &b) in m.iter().enumerate() {
                let product = a as u128 * b as u128;
                out[i + j] += product & u64::MAX as u128;
                out[i + j + 1] += product >> 64;
            }
        }
        for i in 0..5 {
            out[i + 1] += out[i] >> 64;
            out[i] &= u64::MAX as u128;
        }
        assert!(out[4] == 0 && out[5] == 0, "past 256 bits");
        Big([out[0] as u64, out[1] as u64, out[2] as u64, out[3] as u64])
    }

    fn le(&self, other: &Big) -> bool {
        for i in (0..4).rev() {
            if self.0[i] != other.0[i] {
                return self.0[i] < other.0[i];
            }
        }
        true
    }
}

fn first_primes(n: usize) -> Vec<u64> {
    let mut primes = Vec::new();
    let mut candidate = 2u64;
    while primes.len() < n {
        if primes.iter().all(|p| candidate % p != 0) {
            primes.push(candidate);
        }
        candidate += 1;
    }
    primes
}

/// FIPS 180-4 sections 4.2.3 and 5.3.5: the first 64 bits of the fractional parts of the cube
/// roots of the first 80 primes, and of the square roots of the first 8: floor(p^(1/3) 2^64)
/// mod 2^64 is floor((p 2^192)^(1/3)) mod 2^64.
#[test]
fn the_constants_are_the_standards_roots() {
    let primes = first_primes(80);
    for (i, &p) in primes.iter().enumerate() {
        assert_eq!(integer_root(&Big::shifted(p, 192), 3) as u64, K[i], "K[{i}], the cube root of {p}");
    }
    for (i, &p) in primes[..8].iter().enumerate() {
        assert_eq!(integer_root(&Big::shifted(p, 128), 2) as u64, IV[i], "IV[{i}], the square root of {p}");
    }
}

/// The second SHA-512: the standard's text, step by step, with its own constants.
fn slow_sha512(message: &[u8], (k, iv): &(Vec<u64>, Vec<u64>)) -> [u8; 64] {
    let mut h = iv.clone();
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 128 != 112 {
        padded.push(0);
    }
    padded.extend_from_slice(&((message.len() as u128) * 8).to_be_bytes());
    for block in padded.chunks(128) {
        let mut w = [0u64; 80];
        for t in 0..16 {
            w[t] = u64::from_be_bytes(block[8 * t..8 * t + 8].try_into().expect("8 bytes"));
        }
        for t in 16..80 {
            let s0 = w[t - 15].rotate_right(1) ^ w[t - 15].rotate_right(8) ^ (w[t - 15] >> 7);
            let s1 = w[t - 2].rotate_right(19) ^ w[t - 2].rotate_right(61) ^ (w[t - 2] >> 6);
            w[t] = s1.wrapping_add(w[t - 7]).wrapping_add(s0).wrapping_add(w[t - 16]);
        }
        let mut v = h.clone();
        for t in 0..80 {
            let (a, b, c, d, e, f, g, hh) = (v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]);
            let t1 = hh
                .wrapping_add(e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(k[t])
                .wrapping_add(w[t]);
            let t2 = (a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39)).wrapping_add((a & b) ^ (a & c) ^ (b & c));
            v = vec![t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        }
        for (word, add) in h.iter_mut().zip(v) {
            *word = word.wrapping_add(add);
        }
    }
    let mut out = [0u8; 64];
    for (bytes, word) in out.chunks_mut(8).zip(h) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Every length 0 to 300 (under Miri, those at the block and padding boundaries), random bytes,
/// one shot and fed in drawn pieces, equal to the second implementation.
#[test]
fn equals_the_slow_one_at_every_length() {
    let lengths: Vec<usize> = if cfg!(miri) { vec![0, 111, 112, 128, 239, 240, 256] } else { (0..=300).collect() };
    let primes = first_primes(80);
    let constants = (
        primes.iter().map(|&p| integer_root(&Big::shifted(p, 192), 3) as u64).collect::<Vec<u64>>(),
        primes[..8].iter().map(|&p| integer_root(&Big::shifted(p, 128), 2) as u64).collect::<Vec<u64>>(),
    );
    blaze_sim::check(Config::cases(if cfg!(miri) { 2 } else { 4 }), |tape| {
        for &len in &lengths {
            let mut input = vec![0u8; len];
            tape.fill(&mut input, "input");
            let want = slow_sha512(&input, &constants);
            assert_eq!(sha512::hash(&input), want, "{len} bytes, one shot");
            let mut fed = Sha512::new();
            let mut rest = &input[..];
            while !rest.is_empty() {
                let piece = 1 + tape.index(rest.len().min(200), "piece");
                fed.update(&rest[..piece]);
                rest = &rest[piece..];
            }
            assert_eq!(fed.finalize(), want, "{len} bytes, in drawn pieces");
        }
    });
}
