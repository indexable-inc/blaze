//! CRC-32C (Castagnoli) in the CPU's CRC instructions: SSE4.2 `crc32q` on x86-64, ARMv8
//! `crc32cx` on aarch64, each run as three independent streams so the instruction's
//! three-cycle latency is hidden (about 8 bytes a cycle, the speed of Intel's `crc_pcl` without
//! its carry-less folding), and slicing-by-8 tables where the CPU has neither. Every path
//! computes the same function, checked against each other in the tests: which one runs is
//! dispatch, not a fallback. The image's page groups, the store's records and index tables
//! are checked with it.

/// The Castagnoli polynomial, reflected.
const POLY: u32 = 0x82F6_3B78;

/// `TABLES[0]` is the byte-at-a-time table; `TABLES[k][b]` is byte `b` followed by `k` zero bytes.
const TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut out = [[0; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        out[0][byte] = crc;
        byte += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut byte = 0;
        while byte < 256 {
            let previous = out[k - 1][byte];
            out[k][byte] = (previous >> 8) ^ out[0][(previous & 0xff) as usize];
            byte += 1;
        }
        k += 1;
    }
    out
}

/// CRC-32C (reflected polynomial 0x82F63B78, initial !0, final xor !0) of `bytes`.
#[inline]
pub fn crc32c(bytes: &[u8]) -> u32 {
    extend(0, bytes)
}

/// The CRC-32C of `a` followed by `bytes`, given `crc == crc32c(a)`.
pub fn extend(crc: u32, bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU has SSE4.2, checked just above.
        return !unsafe { x86::raw(!crc, bytes) };
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the CPU has the CRC instructions, checked just above.
        return !unsafe { arm::raw(!crc, bytes) };
    }
    !portable(!crc, bytes)
}

fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
}

/// The register (no initial or final inversion) after `bytes`, starting from `state`.
fn portable(mut state: u32, bytes: &[u8]) -> u32 {
    let mut words = bytes.chunks_exact(8);
    for bytes in &mut words {
        let v = word(bytes) ^ u64::from(state);
        let at = |k: u32| ((v >> (8 * k)) & 0xff) as usize;
        state = TABLES[7][at(0)]
            ^ TABLES[6][at(1)]
            ^ TABLES[5][at(2)]
            ^ TABLES[4][at(3)]
            ^ TABLES[3][at(4)]
            ^ TABLES[2][at(5)]
            ^ TABLES[1][at(6)]
            ^ TABLES[0][at(7)];
    }
    for &byte in words.remainder() {
        state = (state >> 8) ^ TABLES[0][((state ^ u32::from(byte)) & 0xff) as usize];
    }
    state
}

/// Bytes per stream in one round of the three-stream loops: a power of two, so [`SHIFT`] is
/// built by squaring. A 64 KiB group is five rounds and a 4 KiB tail, which takes two rounds of
/// [`SMALL`] and 1 KiB on one stream.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const STREAM: usize = 4096;

/// Bytes per stream of the rounds after the [`STREAM`] ones, so that at most `3 * SMALL` bytes
/// run on one stream at the instruction's latency, not its throughput.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SMALL: usize = 512;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = assert!(STREAM.is_power_of_two() && SMALL.is_power_of_two() && SMALL % 8 == 0);

/// What [`STREAM`] zero bytes do to a register: a linear map over GF(2) (multiplication by
/// x^(8·STREAM) mod the polynomial), split by input byte, so applying it is four lookups.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SHIFT: Shift = shift_table(STREAM);

/// [`SHIFT`] for [`SMALL`] zero bytes.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SHIFT_SMALL: Shift = shift_table(SMALL);

/// Each round's stream length with what that many zero bytes do, longest first.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const ROUNDS: [(usize, &Shift); 2] = [(STREAM, &SHIFT), (SMALL, &SHIFT_SMALL)];

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
type Shift = [[u32; 256]; 4];

/// The image of `v` under the linear map whose image of bit `i` is `columns[i]`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const fn apply(columns: &[u32; 32], v: u32) -> u32 {
    let mut out = 0;
    let mut bit = 0;
    while bit < 32 {
        if (v >> bit) & 1 == 1 {
            out ^= columns[bit];
        }
        bit += 1;
    }
    out
}

/// What `stream` zero bytes do to a register, `stream` a power of two.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const fn shift_table(stream: usize) -> Shift {
    let mut columns = [0; 32];
    let mut bit = 0;
    while bit < 32 {
        let state = 1u32 << bit;
        columns[bit] = (state >> 8) ^ TABLES[0][(state & 0xff) as usize];
        bit += 1;
    }
    let mut bytes = 1;
    while bytes < stream {
        let mut squared = [0; 32];
        let mut bit = 0;
        while bit < 32 {
            squared[bit] = apply(&columns, columns[bit]);
            bit += 1;
        }
        columns = squared;
        bytes *= 2;
    }
    let mut out = [[0; 256]; 4];
    let mut k = 0;
    while k < 4 {
        let mut byte = 0;
        while byte < 256 {
            out[k][byte] = apply(&columns, (byte as u32) << (8 * k));
            byte += 1;
        }
        k += 1;
    }
    out
}

/// The register after the zero bytes `table` is of, starting from `state`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn shift(table: &Shift, state: u32) -> u32 {
    table[0][(state & 0xff) as usize]
        ^ table[1][((state >> 8) & 0xff) as usize]
        ^ table[2][((state >> 16) & 0xff) as usize]
        ^ table[3][(state >> 24) as usize]
}

/// The register after three consecutive blocks of the length `table` is of, from the registers
/// of each run alone: `a` from the real starting state, `b` and `c` from 0. A register is linear
/// in its start and its bytes, so a block's run from 0 adds onto the shifted register before it.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn combine(table: &Shift, a: u32, b: u32, c: u32) -> u32 {
    shift(table, shift(table, a) ^ b) ^ c
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    use super::{ROUNDS, combine, word};

    /// The register after `bytes`, starting from `state`.
    #[target_feature(enable = "sse4.2")]
    pub(super) fn raw(mut state: u32, mut bytes: &[u8]) -> u32 {
        for (stream, table) in ROUNDS {
            while bytes.len() >= 3 * stream {
                let (a, rest) = bytes.split_at(stream);
                let (b, rest) = rest.split_at(stream);
                let (c, rest) = rest.split_at(stream);
                let (mut x, mut y, mut z) = (u64::from(state), 0, 0);
                for ((a, b), c) in a.chunks_exact(8).zip(b.chunks_exact(8)).zip(c.chunks_exact(8)) {
                    x = _mm_crc32_u64(x, word(a));
                    y = _mm_crc32_u64(y, word(b));
                    z = _mm_crc32_u64(z, word(c));
                }
                state = combine(table, x as u32, y as u32, z as u32);
                bytes = rest;
            }
        }
        let mut words = bytes.chunks_exact(8);
        let mut wide = u64::from(state);
        for bytes in &mut words {
            wide = _mm_crc32_u64(wide, word(bytes));
        }
        let mut state = wide as u32;
        for &byte in words.remainder() {
            state = _mm_crc32_u8(state, byte);
        }
        state
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    use super::{ROUNDS, combine, word};

    /// The register after `bytes`, starting from `state`.
    #[target_feature(enable = "crc")]
    pub(super) fn raw(mut state: u32, mut bytes: &[u8]) -> u32 {
        for (stream, table) in ROUNDS {
            while bytes.len() >= 3 * stream {
                let (a, rest) = bytes.split_at(stream);
                let (b, rest) = rest.split_at(stream);
                let (c, rest) = rest.split_at(stream);
                let (mut x, mut y, mut z) = (state, 0, 0);
                for ((a, b), c) in a.chunks_exact(8).zip(b.chunks_exact(8)).zip(c.chunks_exact(8)) {
                    x = __crc32cd(x, word(a));
                    y = __crc32cd(y, word(b));
                    z = __crc32cd(z, word(c));
                }
                state = combine(table, x, y, z);
                bytes = rest;
            }
        }
        let mut words = bytes.chunks_exact(8);
        for bytes in &mut words {
            state = __crc32cd(state, word(bytes));
        }
        for &byte in words.remainder() {
            state = __crc32cb(state, byte);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::*;
    use crate::rng::SplitMix64;

    /// The reference every path is checked against: the bitwise definition, one bit a step.
    fn bitwise(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ POLY } else { crc >> 1 };
            }
        }
        !crc
    }

    fn extend_portable(crc: u32, bytes: &[u8]) -> u32 {
        !portable(!crc, bytes)
    }

    #[cfg(target_arch = "x86_64")]
    fn extend_sse42(crc: u32, bytes: &[u8]) -> u32 {
        assert!(std::arch::is_x86_feature_detected!("sse4.2"));
        // SAFETY: the CPU has SSE4.2, asserted just above.
        !unsafe { x86::raw(!crc, bytes) }
    }

    #[cfg(target_arch = "aarch64")]
    fn extend_crc(crc: u32, bytes: &[u8]) -> u32 {
        assert!(std::arch::is_aarch64_feature_detected!("crc"));
        // SAFETY: the CPU has the CRC instructions, asserted just above.
        !unsafe { arm::raw(!crc, bytes) }
    }

    /// Every path this CPU can run, the portable one first, for the differential tests.
    fn paths() -> Vec<(&'static str, fn(u32, &[u8]) -> u32)> {
        let mut out: Vec<(&'static str, fn(u32, &[u8]) -> u32)> = vec![("portable", extend_portable)];
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("sse4.2") {
            out.push(("sse4.2", extend_sse42));
        }
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("crc") {
            out.push(("crc", extend_crc));
        }
        out
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut out = vec![0; len];
        SplitMix64::new(seed).fill(&mut out);
        out
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(&[0; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xff; 32]), 0x62A8_AB43);
        for (name, path) in paths() {
            assert_eq!(path(0, b"123456789"), 0xE306_9283, "{name}");
            assert_eq!(path(0, b""), 0, "{name}");
            assert_eq!(path(0, &[0; 32]), 0x8A91_36AA, "{name}");
        }
    }

    #[test]
    fn every_path_is_the_bitwise_definition() {
        let buffer = noise(300 + 16, 1);
        let mut rng = SplitMix64::new(2);
        for offset in 0..16 {
            for len in 0..=300 {
                let bytes = &buffer[offset..offset + len];
                let want = bitwise(bytes);
                let start = rng.next_u32();
                assert_eq!(crc32c(bytes), want, "{offset}+{len}");
                for (name, path) in paths() {
                    assert_eq!(path(0, bytes), want, "{name}: {offset}+{len}");
                    assert_eq!(
                        path(start, bytes),
                        extend_portable(start, bytes),
                        "{name}: {offset}+{len} from {start:#x}"
                    );
                }
            }
        }
    }

    /// Lengths around both sizes of three-stream rounds, whole groups of every size the image
    /// uses, and the lengths of the store's longer records.
    #[test]
    fn every_path_agrees_on_groups() {
        let lens = [
            1535, 1536, 1537, 3072 + 9, 4096, 10_000, 12_287, 12_288, 12_289, 12_288 + 1536, 16_384,
            24_576 + 5, 40_000, 65_536, 65_536 + 3, 100_001, 1 << 20,
        ];
        let buffer = noise((1 << 20) + 16, 3);
        let mut rng = SplitMix64::new(4);
        for offset in [0, 1, 3, 7, 8, 15] {
            for len in lens {
                let bytes = &buffer[offset..offset + len];
                let start = rng.next_u32();
                let want = extend_portable(start, bytes);
                for (name, path) in paths() {
                    assert_eq!(path(0, bytes), extend_portable(0, bytes), "{name}: {offset}+{len}");
                    assert_eq!(path(start, bytes), want, "{name}: {offset}+{len} from {start:#x}");
                }
            }
        }
        assert_eq!(crc32c(&buffer[..65_536]), bitwise(&buffer[..65_536]));
    }

    #[test]
    fn extend_continues_a_crc() {
        let bytes = noise(50_000, 5);
        let mut rng = SplitMix64::new(6);
        let whole = crc32c(&bytes);
        for _ in 0..200 {
            let at = rng.index(bytes.len() + 1);
            let (a, b) = bytes.split_at(at);
            assert_eq!(extend(crc32c(a), b), whole, "split at {at}");
            for (name, path) in paths() {
                assert_eq!(path(path(0, a), b), whole, "{name}: split at {at}");
            }
        }
        for at in 0..=64 {
            let (a, b) = bytes[..64].split_at(at);
            assert_eq!(extend(crc32c(a), b), crc32c(&bytes[..64]), "split at {at}");
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn shift_is_stream_zero_bytes() {
        let mut rng = SplitMix64::new(7);
        for (stream, table) in ROUNDS {
            let zeros = vec![0u8; stream];
            for state in [0, 1, !0, 0x8000_0000].into_iter().chain((0..100).map(|_| rng.next_u32())) {
                assert_eq!(shift(table, state), portable(state, &zeros), "{stream}: {state:#x}");
            }
        }
    }

    /// Any one flipped bit of a group changes its CRC: what makes every corruption of one bit
    /// a corruption the image and the store name.
    #[test]
    fn one_flipped_bit_changes_the_crc() {
        let mut bytes = noise(4096, 8);
        let whole = crc32c(&bytes);
        for bit in (0..4096 * 8).step_by(13) {
            bytes[bit / 8] ^= 1 << (bit % 8);
            assert_ne!(crc32c(&bytes), whole, "bit {bit}");
            bytes[bit / 8] ^= 1 << (bit % 8);
        }
    }

    #[test]
    fn a_crc_allocates_nothing() {
        let bytes = noise(70_000, 9);
        let crc = blaze_alloc_count::assert_at_most(0, "crc32c", || crc32c(&bytes));
        assert_eq!(crc, extend_portable(0, &bytes));
    }

    #[hegel::test]
    fn matches_the_bitwise_definition(tc: TestCase) {
        let bytes: Vec<u8> = tc.draw(gs::binary().max_size(39_999));
        let at: usize = tc.draw(gs::integers().max_value(bytes.len()));
        let want = bitwise(&bytes);
        assert_eq!(crc32c(&bytes), want);
        let (a, b) = bytes.split_at(at);
        assert_eq!(extend(crc32c(a), b), want);
        for (name, path) in paths() {
            assert_eq!(path(0, &bytes), want, "{}", name);
        }
    }
}
