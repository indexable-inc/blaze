//! blaze: what a provider's seed gives, as pure functions of it
//! (docs/v2/plan/design/determinism.md, section A).
//!
//! The one home of the derivation. std uses it (`super`), and blaze includes this same file
//! (`blaze_usercode::provider::derive`, by `#[path]`) for the sandbox's answers, the wasm
//! host's and its tests, so the two can't disagree. It names nothing but `core`, makes no
//! system call, allocates nothing and has no `unsafe`.
//!
//! A seed is 32 bytes. Everything derived from it is SipHash-1-3, keyed by the seed's first
//! half, of five words: the seed's second half, a domain, and two numbers the domain gives a
//! meaning. SipHash is a keyed pseudo-random function: without the seed its outputs tell
//! nothing of each other, which is all `RandomState` asks of its keys.

/// A provider's seed.
pub type Seed = [u8; 32];

/// The variable a process is given its provider in: `1:<seed as 64 hex digits>:<wall>`, where
/// `<wall>` is `os` or the seconds since the Unix epoch that `SystemTime::now` gives.
pub const VAR: &str = "__BLAZE_PROVIDER";

/// The longest value of [`VAR`]: the version and its colon, the seed, a colon and an `i64`.
pub const VALUE_MOST: usize = 2 + 64 + 1 + 20;

/// The keys of a thread's `RandomState`s: the thread's id, then 0 or 1 for the key.
pub const HASH: u64 = 1;
/// std's `fill_bytes` on a thread: the thread's id, then the block.
pub const FILL: u64 = 2;
/// What the sandbox answers a process's `getrandom` with: the process, then the block.
pub const SYSTEM: u64 = 3;
/// The seed of a child: the spawn's index, then the word of the seed.
pub const CHILD: u64 = 4;
/// What the sandbox gives one open of `/dev/urandom` or `/dev/random`: the open's index, then
/// the block.
pub const DEVICE: u64 = 5;

const fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

const fn le(seed: &Seed, at: usize) -> u64 {
    u64::from_le_bytes([
        seed[at],
        seed[at + 1],
        seed[at + 2],
        seed[at + 3],
        seed[at + 4],
        seed[at + 5],
        seed[at + 6],
        seed[at + 7],
    ])
}

/// One word of `seed` for `domain`, `a` and `b`: SipHash-1-3 keyed by the seed's first sixteen
/// bytes over its last sixteen, `domain`, `a` and `b`, each a little-endian word.
pub const fn word(seed: &Seed, domain: u64, a: u64, b: u64) -> u64 {
    let (k0, k1) = (le(seed, 0), le(seed, 8));
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let message = [le(seed, 16), le(seed, 24), domain, a, b];
    let mut at = 0;
    while at < message.len() {
        v[3] ^= message[at];
        round(&mut v);
        v[0] ^= message[at];
        at += 1;
    }
    // The last word: the message's length, forty bytes, in its top byte, and no tail.
    let last = 40u64 << 56;
    v[3] ^= last;
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    round(&mut v);
    round(&mut v);
    round(&mut v);
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// The bytes of the stream `stream` of `domain` from its byte `offset` on, into `into`: the
/// stream is its blocks' words, little-endian, one after another, so what a reader gets does
/// not depend on how it cut its reads.
pub fn fill(seed: &Seed, domain: u64, stream: u64, offset: u64, into: &mut [u8]) {
    let mut block = offset / 8;
    let mut skip = (offset % 8) as usize;
    let mut at = 0;
    while at < into.len() {
        let bytes = word(seed, domain, stream, block).to_le_bytes();
        let take = if into.len() - at < 8 - skip { into.len() - at } else { 8 - skip };
        let mut i = 0;
        while i < take {
            into[at + i] = bytes[skip + i];
            i += 1;
        }
        at += take;
        skip = 0;
        block = block.wrapping_add(1);
    }
}

/// The two keys of the `RandomState`s of the thread `thread`.
pub const fn keys(seed: &Seed, thread: u64) -> (u64, u64) {
    (word(seed, HASH, thread, 0), word(seed, HASH, thread, 1))
}

/// The seed of the `index`th child a process of `seed` starts: another seed for each index,
/// and the same one for the same index.
pub const fn child(seed: &Seed, index: u64) -> Seed {
    let mut out = [0u8; 32];
    let mut part = 0;
    while part < 4 {
        let bytes = word(seed, CHILD, index, part as u64).to_le_bytes();
        let mut i = 0;
        while i < 8 {
            out[part * 8 + i] = bytes[i];
            i += 1;
        }
        part += 1;
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The value of [`VAR`] for `seed` and `wall` (`None`: the system's clock), written at the
/// start of `out`: how many bytes it is.
pub fn encode(seed: &Seed, wall: Option<i64>, out: &mut [u8; VALUE_MOST]) -> usize {
    out[0] = b'1';
    out[1] = b':';
    let mut at = 2;
    for byte in seed {
        out[at] = HEX[(byte >> 4) as usize];
        out[at + 1] = HEX[(byte & 15) as usize];
        at += 2;
    }
    out[at] = b':';
    at += 1;
    match wall {
        None => {
            out[at] = b'o';
            out[at + 1] = b's';
            at + 2
        }
        Some(seconds) => {
            // The digits, last first, then turned around: at most nineteen and a sign.
            let mut digits = [0u8; 20];
            let mut len = 0;
            let mut left = seconds.unsigned_abs();
            loop {
                digits[len] = b'0' + (left % 10) as u8;
                len += 1;
                left /= 10;
                if left == 0 {
                    break;
                }
            }
            if seconds < 0 {
                digits[len] = b'-';
                len += 1;
            }
            let mut i = 0;
            while i < len {
                out[at + i] = digits[len - 1 - i];
                i += 1;
            }
            at + len
        }
    }
}

const fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// The seed and the wall clock a value of [`VAR`] names (`None`: the system's clock); `None`
/// for a value that is not one this version writes, whole.
pub fn decode(value: &[u8]) -> Option<(Seed, Option<i64>)> {
    if value.len() < 2 + 64 + 1 + 1 || value.len() > VALUE_MOST {
        return None;
    }
    if value[0] != b'1' || value[1] != b':' || value[66] != b':' {
        return None;
    }
    let mut seed = [0u8; 32];
    let mut at = 0;
    while at < 32 {
        let high = nibble(value[2 + 2 * at])?;
        let low = nibble(value[3 + 2 * at])?;
        seed[at] = high << 4 | low;
        at += 1;
    }
    let wall = &value[67..];
    if wall.len() == 2 && wall[0] == b'o' && wall[1] == b's' {
        return Some((seed, None));
    }
    let (negative, digits) = match wall[0] {
        b'-' => (true, &wall[1..]),
        _ => (false, wall),
    };
    if digits.is_empty() {
        return None;
    }
    // Gathered below zero, where an `i64` has one value more: its least parses too.
    let mut seconds: i64 = 0;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        seconds = seconds.checked_mul(10)?.checked_sub((digit - b'0') as i64)?;
    }
    match negative {
        true => Some((seed, Some(seconds))),
        false => Some((seed, Some(seconds.checked_neg()?))),
    }
}
