//! RFC 9562 UUIDs (v4 random, v7 Unix-millisecond time-ordered, and deterministic v8 via BLAKE3)
//! with zero ambient RNG or clock access, zero integer `as` casts, and zero panicking indexes.

use core::fmt;

use crate::hex::{hex_nibble, lower_nibble};

/// A 128-bit Universally Unique Identifier (RFC 9562), stored in big-endian network byte order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct Uuid([u8; 16]);

/// RFC 9562 UUID variant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// Reserved for NCS backward compatibility (`0xxx`).
    Ncs,
    /// RFC 9562 / RFC 4122 standard variant (`10xx`).
    Rfc,
    /// Reserved for Microsoft backward compatibility (`110x`).
    Microsoft,
    /// Reserved for future definition (`111x`).
    Future,
}

/// Why parsing a UUID string failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UuidError {
    /// Input length did not match a valid UUID encoding.
    InvalidLength { len: usize },
    /// Expected a `-` hyphen at `offset`.
    ExpectedHyphen { offset: usize, found: u8 },
    /// Non-hex byte at `offset`.
    InvalidHexByte { offset: usize, byte: u8 },
    /// Expected closing `}` for a braced UUID.
    UnclosedBrace,
}

impl fmt::Display for UuidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            UuidError::InvalidLength { len } => {
                write!(f, "bstd::uuid: invalid UUID length ({len} bytes)")
            }
            UuidError::ExpectedHyphen { offset, found } => {
                write!(f, "bstd::uuid: expected `-` at byte offset {offset}, found 0x{found:02x}")
            }
            UuidError::InvalidHexByte { offset, byte } => {
                write!(f, "bstd::uuid: invalid hex byte 0x{byte:02x} at byte offset {offset}")
            }
            UuidError::UnclosedBrace => f.write_str("bstd::uuid: missing closing `}`"),
        }
    }
}

impl core::error::Error for UuidError {}

impl Uuid {
    /// The Nil UUID (`00000000-0000-0000-0000-000000000000`).
    pub const NIL: Self = Self([0u8; 16]);

    /// The Max UUID (`ffffffff-ffff-ffff-ffff-ffffffffffff`).
    pub const MAX: Self = Self([0xffu8; 16]);

    /// Construct a UUID from raw big-endian bytes without modifying version or variant bits.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Construct a UUID from a big-endian `u128`.
    #[inline]
    #[must_use]
    pub const fn from_u128(value: u128) -> Self {
        Self(value.to_be_bytes())
    }

    /// Return the underlying 16 big-endian bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Return the underlying 16 bytes by value.
    #[inline]
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Return the UUID as a big-endian `u128`.
    #[inline]
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        u128::from_be_bytes(self.0)
    }

    /// Build an RFC 9562 UUIDv4 (random) from 16 random bytes, setting version 4 and RFC variant.
    #[inline]
    #[must_use]
    pub const fn v4_from_bytes(bytes: [u8; 16]) -> Self {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = bytes;
        Self([
            b0,
            b1,
            b2,
            b3,
            b4,
            b5,
            (b6 & 0x0f) | 0x40,
            b7,
            (b8 & 0x3f) | 0x80,
            b9,
            b10,
            b11,
            b12,
            b13,
            b14,
            b15,
        ])
    }

    /// Generate an RFC 9562 UUIDv4 using the caller's [`crate::rng::Rng`].
    #[inline]
    pub fn v4(rng: &mut crate::rng::Rng) -> Self {
        let mut bytes = [0u8; 16];
        rng.fill(&mut bytes);
        Self::v4_from_bytes(bytes)
    }

    /// Build an RFC 9562 UUIDv7 (time-ordered) from a 48-bit Unix epoch millisecond timestamp
    /// (`unix_ms`) and 10 random bytes, setting version 7 and RFC variant.
    #[inline]
    #[must_use]
    pub const fn v7(unix_ms: u64, rand: [u8; 10]) -> Self {
        Self::v7_planted(unix_ms, rand, false)
    }

    const fn v7_planted(unix_ms: u64, rand: [u8; 10], plant_bad_version: bool) -> Self {
        let [_, _, t0, t1, t2, t3, t4, t5] = unix_ms.to_be_bytes();
        let [r0, r1, r2, r3, r4, r5, r6, r7, r8, r9] = rand;
        let ver_bits = if plant_bad_version { 0x40 } else { 0x70 };
        Self([
            t0,
            t1,
            t2,
            t3,
            t4,
            t5,
            (r0 & 0x0f) | ver_bits,
            r1,
            (r2 & 0x3f) | 0x80,
            r3,
            r4,
            r5,
            r6,
            r7,
            r8,
            r9,
        ])
    }

    /// Generate an RFC 9562 UUIDv7 from `unix_ms` and the caller's [`crate::rng::Rng`].
    #[inline]
    pub fn v7_with(unix_ms: u64, rng: &mut crate::rng::Rng) -> Self {
        let mut rand = [0u8; 10];
        rng.fill(&mut rand);
        Self::v7(unix_ms, rand)
    }

    /// Deterministic RFC 9562 UUIDv8 derived from domain-separated BLAKE3 of `data`.
    #[must_use]
    pub fn from_blake3(domain: &str, data: &[u8]) -> Self {
        let mut hasher = crate::digest::Blake3::new();
        hasher.update(domain.as_bytes());
        hasher.update(&[0u8]);
        hasher.update(data);
        let digest = hasher.finalize();
        let mut b = [0u8; 16];
        if let Some(prefix) = digest.get(..16) {
            b.copy_from_slice(prefix);
        }
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = b;
        Self([
            b0,
            b1,
            b2,
            b3,
            b4,
            b5,
            (b6 & 0x0f) | 0x80,
            b7,
            (b8 & 0x3f) | 0x80,
            b9,
            b10,
            b11,
            b12,
            b13,
            b14,
            b15,
        ])
    }

    /// Return the 4-bit version field (`4`, `7`, `8`, ...).
    #[inline]
    #[must_use]
    pub const fn version(&self) -> u8 {
        let [_, _, _, _, _, _, b6, ..] = self.0;
        b6 >> 4
    }

    /// Return the RFC 9562 [`Variant`].
    #[inline]
    #[must_use]
    pub const fn variant(&self) -> Variant {
        let [_, _, _, _, _, _, _, _, b8, ..] = self.0;
        if (b8 & 0x80) == 0 {
            Variant::Ncs
        } else if (b8 & 0xc0) == 0x80 {
            Variant::Rfc
        } else if (b8 & 0xe0) == 0xc0 {
            Variant::Microsoft
        } else {
            Variant::Future
        }
    }

    /// Extract the 48-bit Unix epoch millisecond timestamp if this is an RFC-variant UUIDv7.
    #[inline]
    #[must_use]
    pub const fn timestamp_ms(&self) -> Option<u64> {
        if self.version() != 7 || !matches!(self.variant(), Variant::Rfc) {
            return None;
        }
        let [b0, b1, b2, b3, b4, b5, ..] = self.0;
        Some(u64::from_be_bytes([0, 0, b0, b1, b2, b3, b4, b5]))
    }

    /// Format as canonical lowercase hyphenated ASCII (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`)
    /// with zero heap allocation.
    #[must_use]
    pub const fn to_hyphenated_bytes(self) -> [u8; 36] {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = self.0;
        [
            lower_nibble(b0 >> 4),
            lower_nibble(b0),
            lower_nibble(b1 >> 4),
            lower_nibble(b1),
            lower_nibble(b2 >> 4),
            lower_nibble(b2),
            lower_nibble(b3 >> 4),
            lower_nibble(b3),
            b'-',
            lower_nibble(b4 >> 4),
            lower_nibble(b4),
            lower_nibble(b5 >> 4),
            lower_nibble(b5),
            b'-',
            lower_nibble(b6 >> 4),
            lower_nibble(b6),
            lower_nibble(b7 >> 4),
            lower_nibble(b7),
            b'-',
            lower_nibble(b8 >> 4),
            lower_nibble(b8),
            lower_nibble(b9 >> 4),
            lower_nibble(b9),
            b'-',
            lower_nibble(b10 >> 4),
            lower_nibble(b10),
            lower_nibble(b11 >> 4),
            lower_nibble(b11),
            lower_nibble(b12 >> 4),
            lower_nibble(b12),
            lower_nibble(b13 >> 4),
            lower_nibble(b13),
            lower_nibble(b14 >> 4),
            lower_nibble(b14),
            lower_nibble(b15 >> 4),
            lower_nibble(b15),
        ]
    }

    /// Format as 32 lowercase hex ASCII bytes without hyphens (`xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`).
    #[must_use]
    pub const fn to_simple_bytes(self) -> [u8; 32] {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = self.0;
        [
            lower_nibble(b0 >> 4),
            lower_nibble(b0),
            lower_nibble(b1 >> 4),
            lower_nibble(b1),
            lower_nibble(b2 >> 4),
            lower_nibble(b2),
            lower_nibble(b3 >> 4),
            lower_nibble(b3),
            lower_nibble(b4 >> 4),
            lower_nibble(b4),
            lower_nibble(b5 >> 4),
            lower_nibble(b5),
            lower_nibble(b6 >> 4),
            lower_nibble(b6),
            lower_nibble(b7 >> 4),
            lower_nibble(b7),
            lower_nibble(b8 >> 4),
            lower_nibble(b8),
            lower_nibble(b9 >> 4),
            lower_nibble(b9),
            lower_nibble(b10 >> 4),
            lower_nibble(b10),
            lower_nibble(b11 >> 4),
            lower_nibble(b11),
            lower_nibble(b12 >> 4),
            lower_nibble(b12),
            lower_nibble(b13 >> 4),
            lower_nibble(b13),
            lower_nibble(b14 >> 4),
            lower_nibble(b14),
            lower_nibble(b15 >> 4),
            lower_nibble(b15),
        ]
    }

    /// Strictly parse a canonical 36-byte hyphenated UUID (`8-4-4-4-12`).
    pub fn parse_hyphenated(input: &str) -> Result<Self, UuidError> {
        parse_hyphenated_with_base(input.as_bytes(), 0)
    }

    /// Parse a UUID in canonical hyphenated (`36`), simple hex (`32`), braced (`{...}`), or URN
    /// (`urn:uuid:...`) form.
    pub fn parse(input: &str) -> Result<Self, UuidError> {
        let bytes = input.as_bytes();
        let (slice, base) = if let Some(rest) = input.strip_prefix("urn:uuid:") {
            (rest.as_bytes(), 9usize)
        } else if bytes.first() == Some(&b'{') {
            let Some(inner) = bytes.get(1..bytes.len().saturating_sub(1)) else {
                return Err(UuidError::UnclosedBrace);
            };
            if bytes.last() != Some(&b'}') || bytes.len() < 2 {
                return Err(UuidError::UnclosedBrace);
            }
            (inner, 1usize)
        } else {
            (bytes, 0usize)
        };

        match slice.len() {
            36 => parse_hyphenated_with_base(slice, base),
            32 => parse_simple_with_base(slice, base),
            _ => Err(UuidError::InvalidLength { len: bytes.len() }),
        }
    }
}

fn parse_hyphenated_with_base(bytes: &[u8], base: usize) -> Result<Uuid, UuidError> {
    if bytes.len() != 36 {
        return Err(UuidError::InvalidLength { len: bytes.len() });
    }
    for &h in &[8usize, 13, 18, 23] {
        let found = bytes.get(h).copied().unwrap_or(0);
        if found != b'-' {
            return Err(UuidError::ExpectedHyphen { offset: base + h, found });
        }
    }
    const GROUPS: [usize; 16] = [0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34];
    let mut out = [0u8; 16];
    for (pos, slot) in GROUPS.iter().copied().zip(out.iter_mut()) {
        let hi_b = bytes.get(pos).copied().unwrap_or(0);
        let lo_b = bytes.get(pos + 1).copied().unwrap_or(0);
        let hi = hex_nibble(hi_b).ok_or(UuidError::InvalidHexByte { offset: base + pos, byte: hi_b })?;
        let lo =
            hex_nibble(lo_b).ok_or(UuidError::InvalidHexByte { offset: base + pos + 1, byte: lo_b })?;
        *slot = (hi << 4) | lo;
    }
    Ok(Uuid(out))
}

fn parse_simple_with_base(bytes: &[u8], base: usize) -> Result<Uuid, UuidError> {
    if bytes.len() != 32 {
        return Err(UuidError::InvalidLength { len: bytes.len() });
    }
    let mut out = [0u8; 16];
    for (i, (pair, slot)) in bytes.chunks_exact(2).zip(out.iter_mut()).enumerate() {
        let [hi_b, lo_b] = *pair else { continue };
        let pos = i * 2;
        let hi = hex_nibble(hi_b).ok_or(UuidError::InvalidHexByte { offset: base + pos, byte: hi_b })?;
        let lo =
            hex_nibble(lo_b).ok_or(UuidError::InvalidHexByte { offset: base + pos + 1, byte: lo_b })?;
        *slot = (hi << 4) | lo;
    }
    Ok(Uuid(out))
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buf = self.to_hyphenated_bytes();
        if let Ok(s) = core::str::from_utf8(&buf) {
            f.write_str(s)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;

    #[test]
    fn uuid_v4_v7_v8_and_parsing() {
        let h = heap();
        let mut rng = crate::rng::Rng::new(42);
        let u4 = Uuid::v4(&mut rng);
        assert_eq!(u4.version(), 4);
        assert_eq!(u4.variant(), Variant::Rfc);
        assert_eq!(u4.timestamp_ms(), None);

        let ts: u64 = 1_763_920_000_123;
        let u7_a = Uuid::v7_with(ts, &mut rng);
        let u7_b = Uuid::v7_with(ts + 1, &mut rng);
        assert_eq!(u7_a.version(), 7);
        assert_eq!(u7_a.variant(), Variant::Rfc);
        assert_eq!(u7_a.timestamp_ms(), Some(ts));
        assert!(u7_a < u7_b);
        assert!(format_in!(h.alloc(), "{u7_a}").as_str() < format_in!(h.alloc(), "{u7_b}").as_str());

        let det1 = Uuid::from_blake3("bstd.test", b"hello");
        let det2 = Uuid::from_blake3("bstd.test", b"hello");
        let det3 = Uuid::from_blake3("bstd.other", b"hello");
        assert_eq!(det1, det2);
        assert_ne!(det1, det3);
        assert_eq!(det1.version(), 8);
        assert_eq!(det1.variant(), Variant::Rfc);

        let s = format_in!(h.alloc(), "{u7_a}");
        assert_eq!(Uuid::parse(s.as_str()), Ok(u7_a));
        assert_eq!(Uuid::parse_hyphenated(s.as_str()), Ok(u7_a));
        assert_eq!(Uuid::parse(format_in!(h.alloc(), "{{{}}}", s.as_str()).as_str()), Ok(u7_a));
        assert_eq!(
            Uuid::parse(format_in!(h.alloc(), "urn:uuid:{}", s.as_str()).as_str()),
            Ok(u7_a)
        );
        let simple = u7_a.to_simple_bytes();
        assert_eq!(Uuid::parse(core::str::from_utf8(&simple).expect("ascii")), Ok(u7_a));
        assert_eq!(
            Uuid::parse("00000000x0000-0000-0000-000000000000"),
            Err(UuidError::ExpectedHyphen { offset: 8, found: b'x' })
        );
    }

    /// The invariant: UUID v7 encoding, parsing, and version/variant bits round-trip and preserve timestamp ordering.
    #[test]
    fn property_uuid_v7_round_trip_and_monotonic_time() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let ms1 = tape.below(1u64 << 48, "ms1");
            let delta = u64::try_from(tape.int_in(1..=10_000, "delta")).unwrap_or(1);
            let ms2 = ms1.saturating_add(delta).min((1u64 << 48) - 1);
            let mut r1 = [0u8; 10];
            let mut r2 = [0u8; 10];
            for b in &mut r1 {
                *b = u8::try_from(tape.below(256, "r1")).unwrap_or(0);
            }
            for b in &mut r2 {
                *b = u8::try_from(tape.below(256, "r2")).unwrap_or(0);
            }
            let u1 = Uuid::v7(ms1, r1);
            let u2 = Uuid::v7(ms2, r2);
            assert_eq!(u1.version(), 7);
            assert_eq!(u1.variant(), Variant::Rfc);
            assert_eq!(u1.timestamp_ms(), Some(ms1));
            if ms1 < ms2 {
                assert!(u1 < u2);
            }
            let formatted = format_in!(h.alloc(), "{u1}");
            assert_eq!(Uuid::parse(formatted.as_str()), Ok(u1));
        });
    }

    #[test]
    fn the_oracle_catches_bad_v7_version() {
        let h = heap();
        blaze_sim::control(Config::cases(64), false, true, "", |tape, plant| {
            let ms = tape.below(1u64 << 48, "ms");
            let mut r = [0u8; 10];
            for b in &mut r {
                *b = u8::try_from(tape.below(256, "r")).unwrap_or(0);
            }
            let u = Uuid::v7_planted(ms, r, plant);
            if u.version() != 7 || u.timestamp_ms() != Some(ms) {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "bad v7 metadata: ver={}", u.version()).as_str().into(),
                ));
            }
            Ok(())
        });
    }
}
