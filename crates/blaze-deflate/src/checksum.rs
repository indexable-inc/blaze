//! The two checksums the framings carry: Adler-32 (RFC 1950 section 8, zlib's trailer) and
//! CRC-32 (ISO-HDLC, the reflected polynomial 0xEDB88320: gzip's trailer, RFC 1952 section 8,
//! and PNG's). Each is a running state fed in pieces; a piece's split never changes the sum.
//!
//! Neither is a check of trust: both catch a corrupted transfer, never a forged one. What a
//! fetched byte is trusted by is its digest (blaze-sha256, blaze-blake3).

#![blaze::sans_io]

/// Adler-32's modulus: the largest prime below 2^16.
const ADLER_MOD: u32 = 65_521;

/// The most bytes summed before the two sums are reduced: the largest `n` with
/// `255 n (n + 1) / 2 + (n + 1) (MOD - 1) <= 2^32 - 1` (zlib's NMAX), so `b` never overflows.
const ADLER_RUN: usize = 5_552;

/// A running Adler-32.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    /// The sum of nothing: 1.
    pub const fn new() -> Adler32 {
        Adler32 { a: 1, b: 0 }
    }

    /// `bytes` added.
    pub fn update(&mut self, bytes: &[u8]) {
        let (mut a, mut b) = (self.a, self.b);
        for run in bytes.chunks(ADLER_RUN) {
            (a, b) = crate::simd::adler_run(a, b, run);
            a %= ADLER_MOD;
            b %= ADLER_MOD;
        }
        self.a = a;
        self.b = b;
    }

    /// The sum of every byte added.
    pub const fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

/// The Adler-32 of `bytes`.
pub fn adler32(bytes: &[u8]) -> u32 {
    let mut sum = Adler32::new();
    sum.update(bytes);
    sum.finish()
}

/// CRC-32's tables for slicing by eight: `TABLES[k][n]` is the CRC of the byte `n` followed by
/// `k` zero bytes, made as the crate compiles.
const TABLES: [[u32; 256]; 8] = crc_tables();

const fn crc_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        tables[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = tables[k - 1][n];
            tables[k][n] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
            k += 1;
        }
        n += 1;
    }
    tables
}

/// A running CRC-32.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Crc32 {
    /// The register, inverted as the algorithm keeps it.
    state: u32,
}

impl Crc32 {
    /// The CRC of nothing: 0.
    pub const fn new() -> Crc32 {
        Crc32 { state: 0xFFFF_FFFF }
    }

    /// `bytes` added, eight at a time.
    pub fn update(&mut self, bytes: &[u8]) {
        let t = &TABLES;
        let mut c = self.state;
        let mut words = bytes.chunks_exact(8);
        for w in &mut words {
            let lo = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ c;
            let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
            c = t[7][usize::from(lo as u8)]
                ^ t[6][usize::from((lo >> 8) as u8)]
                ^ t[5][usize::from((lo >> 16) as u8)]
                ^ t[4][usize::from((lo >> 24) as u8)]
                ^ t[3][usize::from(hi as u8)]
                ^ t[2][usize::from((hi >> 8) as u8)]
                ^ t[1][usize::from((hi >> 16) as u8)]
                ^ t[0][usize::from((hi >> 24) as u8)];
        }
        for &byte in words.remainder() {
            c = (c >> 8) ^ t[0][usize::from((c as u8) ^ byte)];
        }
        self.state = c;
    }

    /// The CRC of every byte added.
    pub const fn finish(&self) -> u32 {
        !self.state
    }
}

/// The CRC-32 of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}
