//! Pure `#![no_std]` one-time passwords (RFC 4226 HOTP, RFC 6238 TOTP, `otpauth://` URIs),
//! RFC 4648 & Crockford Base32 encoding/decoding, and RFC 7468 PEM armor.
//!
//! - [`base32`]: RFC 4648 Base32 (padded or unpadded, case-insensitive decode, strict trailing-bit
//!   rejection) and Douglas Crockford's Base32 (`-` separators, `I`/`L` -> `1`, `O` -> `0`).
//! - [`pem`]: strict RFC 7468 PEM armor parser and 64-character-wrapped encoder over
//!   `blaze_base64::STANDARD`.
//! - [`hotp`], [`totp`], [`verify_totp`], [`format_otpauth_uri_in`]: RFC 4226 / RFC 6238 OTP
//!   generation and constant-time verification with an explicit clock parameter (zero ambient
//!   clock). Supports SHA-1 (FIPS 180-4) and SHA-256 (`crate::digest::hmac_sha256`).
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_own::{AllocFailure, String};

use crate::digest::{ct_eq, hmac_sha256};
use crate::query::percent_encode_into;

/// Maximum clock-skew window (in steps on each side) accepted by [`verify_totp`].
pub const MAX_SKEW_STEPS: u64 = 1_024;

// ---------------------------------------------------------------------------
// Fallible fmt::Write adapter for blaze_own::String
// ---------------------------------------------------------------------------

struct TryStringWriter<'a, A: Allocator> {
    out: &'a mut String<A>,
    err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.err.is_some() {
            return Err(fmt::Error);
        }
        match self.out.try_push_str(s) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.err = Some(e);
                Err(fmt::Error)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RFC 4648 & Crockford Base32
// ---------------------------------------------------------------------------

/// RFC 4648 §6 Base32 and Douglas Crockford's Base32 encoding and decoding.
pub mod base32 {
    use core::alloc::Allocator;
    use core::fmt;

    use blaze_own::{AllocFailure, String, Vec};

    const RFC4648_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// Why Base32 decoding failed.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Base32Error {
        /// Byte at `offset` is not a valid symbol of the Base32 alphabet.
        InvalidByte { offset: usize, byte: u8 },
        /// `=` padding at `offset` is misplaced, malformed, or has an invalid length.
        InvalidPadding { offset: usize },
        /// Input symbol count `len` cannot represent a whole number of bytes (`len % 8` in
        /// `{1, 3, 6}`).
        InvalidLength { len: usize },
        /// Final symbol at `offset` has non-zero unused trailing bits (RFC 4648 §3.5).
        NonZeroTrailingBits { offset: usize },
        /// Backing allocator refused memory.
        Alloc(AllocFailure),
    }

    impl From<AllocFailure> for Base32Error {
        fn from(err: AllocFailure) -> Self {
            Base32Error::Alloc(err)
        }
    }

    impl fmt::Display for Base32Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match *self {
                Base32Error::InvalidByte { offset, byte } => {
                    write!(f, "bstd::otp::base32: invalid byte {byte:#04x} at offset {offset}")
                }
                Base32Error::InvalidPadding { offset } => {
                    write!(f, "bstd::otp::base32: invalid padding at offset {offset}")
                }
                Base32Error::InvalidLength { len } => {
                    write!(f, "bstd::otp::base32: invalid Base32 symbol count {len}")
                }
                Base32Error::NonZeroTrailingBits { offset } => {
                    write!(
                        f,
                        "bstd::otp::base32: non-zero trailing bits at offset {offset} (non-canonical)"
                    )
                }
                Base32Error::Alloc(err) => {
                    write!(f, "bstd::otp::base32: allocation failed ({err})")
                }
            }
        }
    }

    impl core::error::Error for Base32Error {}

    #[inline]
    fn symbol(alphabet: &[u8; 32], value: u8) -> u8 {
        alphabet.get(usize::from(value & 0x1f)).copied().unwrap_or(b'A')
    }

    #[inline]
    fn encoded_len(len: usize, pad: bool) -> usize {
        let full = (len / 5).saturating_mul(8);
        let tail = match len % 5 {
            0 => 0,
            1 => {
                if pad {
                    8
                } else {
                    2
                }
            }
            2 => {
                if pad {
                    8
                } else {
                    4
                }
            }
            3 => {
                if pad {
                    8
                } else {
                    5
                }
            }
            _ => {
                if pad {
                    8
                } else {
                    7
                }
            }
        };
        full.saturating_add(tail)
    }

    fn encode_with_alphabet_in<A: Allocator + Copy>(
        bytes: &[u8],
        alphabet: &[u8; 32],
        pad: bool,
        alloc: A,
    ) -> Result<String<A>, AllocFailure> {
        let mut out = String::try_with_capacity_in(encoded_len(bytes.len(), pad), alloc)?;
        let mut chunks = bytes.chunks_exact(5);
        for chunk in chunks.by_ref() {
            if let [b0, b1, b2, b3, b4] = *chunk {
                let encoded = [
                    symbol(alphabet, b0 >> 3),
                    symbol(alphabet, ((b0 & 0x07) << 2) | (b1 >> 6)),
                    symbol(alphabet, (b1 >> 1) & 0x1f),
                    symbol(alphabet, ((b1 & 0x01) << 4) | (b2 >> 4)),
                    symbol(alphabet, ((b2 & 0x0f) << 1) | (b3 >> 7)),
                    symbol(alphabet, (b3 >> 2) & 0x1f),
                    symbol(alphabet, ((b3 & 0x03) << 3) | (b4 >> 5)),
                    symbol(alphabet, b4 & 0x1f),
                ];
                if let Ok(s) = core::str::from_utf8(&encoded) {
                    out.try_push_str(s)?;
                }
            }
        }
        match *chunks.remainder() {
            [] => {}
            [b0] => {
                let encoded = [symbol(alphabet, b0 >> 3), symbol(alphabet, (b0 & 0x07) << 2)];
                if let Ok(s) = core::str::from_utf8(&encoded) {
                    out.try_push_str(s)?;
                }
                if pad {
                    out.try_push_str("======")?;
                }
            }
            [b0, b1] => {
                let encoded = [
                    symbol(alphabet, b0 >> 3),
                    symbol(alphabet, ((b0 & 0x07) << 2) | (b1 >> 6)),
                    symbol(alphabet, (b1 >> 1) & 0x1f),
                    symbol(alphabet, (b1 & 0x01) << 4),
                ];
                if let Ok(s) = core::str::from_utf8(&encoded) {
                    out.try_push_str(s)?;
                }
                if pad {
                    out.try_push_str("====")?;
                }
            }
            [b0, b1, b2] => {
                let encoded = [
                    symbol(alphabet, b0 >> 3),
                    symbol(alphabet, ((b0 & 0x07) << 2) | (b1 >> 6)),
                    symbol(alphabet, (b1 >> 1) & 0x1f),
                    symbol(alphabet, ((b1 & 0x01) << 4) | (b2 >> 4)),
                    symbol(alphabet, (b2 & 0x0f) << 1),
                ];
                if let Ok(s) = core::str::from_utf8(&encoded) {
                    out.try_push_str(s)?;
                }
                if pad {
                    out.try_push_str("===")?;
                }
            }
            [b0, b1, b2, b3] => {
                let encoded = [
                    symbol(alphabet, b0 >> 3),
                    symbol(alphabet, ((b0 & 0x07) << 2) | (b1 >> 6)),
                    symbol(alphabet, (b1 >> 1) & 0x1f),
                    symbol(alphabet, ((b1 & 0x01) << 4) | (b2 >> 4)),
                    symbol(alphabet, ((b2 & 0x0f) << 1) | (b3 >> 7)),
                    symbol(alphabet, (b3 >> 2) & 0x1f),
                    symbol(alphabet, (b3 & 0x03) << 3),
                ];
                if let Ok(s) = core::str::from_utf8(&encoded) {
                    out.try_push_str(s)?;
                }
                if pad {
                    out.try_push('=')?;
                }
            }
            _ => {}
        }
        Ok(out)
    }

    #[inline]
    const fn decode_rfc4648_byte(b: u8, offset: usize) -> Result<u8, Base32Error> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a'),
            b'2'..=b'7' => Ok(b - b'2' + 26),
            _ => Err(Base32Error::InvalidByte { offset, byte: b }),
        }
    }

    #[inline]
    const fn decode_crockford_byte(b: u8, offset: usize) -> Result<u8, Base32Error> {
        match b {
            b'0' | b'O' | b'o' => Ok(0),
            b'1' | b'I' | b'i' | b'L' | b'l' => Ok(1),
            b'2'..=b'9' => Ok(b - b'0'),
            b'A'..=b'H' => Ok(b - b'A' + 10),
            b'a'..=b'h' => Ok(b - b'a' + 10),
            b'J'..=b'K' => Ok(b - b'J' + 18),
            b'j'..=b'k' => Ok(b - b'j' + 18),
            b'M'..=b'N' => Ok(b - b'M' + 20),
            b'm'..=b'n' => Ok(b - b'm' + 20),
            b'P'..=b'T' => Ok(b - b'P' + 22),
            b'p'..=b't' => Ok(b - b'p' + 22),
            b'V'..=b'Z' => Ok(b - b'V' + 27),
            b'v'..=b'z' => Ok(b - b'v' + 27),
            _ => Err(Base32Error::InvalidByte { offset, byte: b }),
        }
    }

    #[inline]
    fn decode_full_block(v: [u8; 8]) -> [u8; 5] {
        let [v0, v1, v2, v3, v4, v5, v6, v7] = v;
        [
            (v0 << 3) | (v1 >> 2),
            (v1 << 6) | (v2 << 1) | (v3 >> 4),
            (v3 << 4) | (v4 >> 1),
            (v4 << 7) | (v5 << 2) | (v6 >> 3),
            (v6 << 5) | v7,
        ]
    }

    fn push_tail_block<A: Allocator>(
        v: [u8; 8],
        count: usize,
        last_offset: usize,
        total_syms: usize,
        out: &mut Vec<u8, A>,
    ) -> Result<(), Base32Error> {
        let [v0, v1, v2, v3, v4, v5, v6, _] = v;
        match count {
            0 => Ok(()),
            2 => {
                if (v1 & 0x03) != 0 {
                    return Err(Base32Error::NonZeroTrailingBits { offset: last_offset });
                }
                out.try_push((v0 << 3) | (v1 >> 2)).map_err(|r| Base32Error::Alloc(r.why))
            }
            4 => {
                if (v3 & 0x0f) != 0 {
                    return Err(Base32Error::NonZeroTrailingBits { offset: last_offset });
                }
                let decoded = [(v0 << 3) | (v1 >> 2), (v1 << 6) | (v2 << 1) | (v3 >> 4)];
                out.try_extend_from_slice(&decoded).map_err(Base32Error::Alloc)
            }
            5 => {
                if (v4 & 0x01) != 0 {
                    return Err(Base32Error::NonZeroTrailingBits { offset: last_offset });
                }
                let decoded = [
                    (v0 << 3) | (v1 >> 2),
                    (v1 << 6) | (v2 << 1) | (v3 >> 4),
                    (v3 << 4) | (v4 >> 1),
                ];
                out.try_extend_from_slice(&decoded).map_err(Base32Error::Alloc)
            }
            7 => {
                if (v6 & 0x07) != 0 {
                    return Err(Base32Error::NonZeroTrailingBits { offset: last_offset });
                }
                let decoded = [
                    (v0 << 3) | (v1 >> 2),
                    (v1 << 6) | (v2 << 1) | (v3 >> 4),
                    (v3 << 4) | (v4 >> 1),
                    (v4 << 7) | (v5 << 2) | (v6 >> 3),
                ];
                out.try_extend_from_slice(&decoded).map_err(Base32Error::Alloc)
            }
            _ => Err(Base32Error::InvalidLength { len: total_syms }),
        }
    }

    /// Encode `bytes` with the RFC 4648 §6 Base32 alphabet (`A-Z`, `2-7`), optionally padded
    /// with `=` to a multiple of 8 characters.
    pub fn encode_rfc4648_in<A: Allocator + Copy>(
        bytes: &[u8],
        pad: bool,
        alloc: A,
    ) -> Result<String<A>, AllocFailure> {
        encode_with_alphabet_in(bytes, RFC4648_ALPHABET, pad, alloc)
    }

    /// Decode RFC 4648 §6 Base32 (case-insensitive, optional `=` padding).
    ///
    /// Strictly rejects invalid characters, misplaced or wrong-length `=` padding, invalid symbol
    /// counts, and non-zero trailing bits.
    pub fn decode_rfc4648_in<A: Allocator + Copy>(
        input: &str,
        alloc: A,
    ) -> Result<Vec<u8, A>, Base32Error> {
        let bytes = input.as_bytes();
        let data = if let Some(pad_pos) = bytes.iter().position(|&b| b == b'=') {
            if let Some(tail) = bytes.get(pad_pos..) {
                for (k, &b) in tail.iter().enumerate() {
                    if b != b'=' {
                        return Err(Base32Error::InvalidPadding { offset: pad_pos + k });
                    }
                }
            }
            let pad_len = bytes.len() - pad_pos;
            if bytes.len() % 8 != 0 || !matches!(pad_len, 1 | 3 | 4 | 6) {
                return Err(Base32Error::InvalidPadding { offset: pad_pos });
            }
            bytes.get(..pad_pos).unwrap_or(&[])
        } else {
            bytes
        };

        let tail_bytes = match data.len() % 8 {
            0 => 0usize,
            2 => 1,
            4 => 2,
            5 => 3,
            7 => 4,
            _ => return Err(Base32Error::InvalidLength { len: bytes.len() }),
        };
        let out_cap = (data.len() / 8).saturating_mul(5).saturating_add(tail_bytes);
        let mut out = Vec::try_with_capacity_in(out_cap, alloc)?;

        let mut chunks = data.chunks_exact(8);
        let mut offset = 0usize;
        for chunk in chunks.by_ref() {
            if let [c0, c1, c2, c3, c4, c5, c6, c7] = *chunk {
                let vals = [
                    decode_rfc4648_byte(c0, offset)?,
                    decode_rfc4648_byte(c1, offset + 1)?,
                    decode_rfc4648_byte(c2, offset + 2)?,
                    decode_rfc4648_byte(c3, offset + 3)?,
                    decode_rfc4648_byte(c4, offset + 4)?,
                    decode_rfc4648_byte(c5, offset + 5)?,
                    decode_rfc4648_byte(c6, offset + 6)?,
                    decode_rfc4648_byte(c7, offset + 7)?,
                ];
                let decoded = decode_full_block(vals);
                out.try_extend_from_slice(&decoded)?;
            }
            offset += 8;
        }

        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut vals = [0u8; 8];
            for (i, &b) in rem.iter().enumerate() {
                let v = decode_rfc4648_byte(b, offset + i)?;
                if let Some(slot) = vals.get_mut(i) {
                    *slot = v;
                }
            }
            let last_offset = offset + rem.len() - 1;
            push_tail_block(vals, rem.len(), last_offset, data.len(), &mut out)?;
        }

        Ok(out)
    }

    /// Encode `bytes` with Douglas Crockford's Base32 alphabet (unpadded, uppercase).
    pub fn encode_crockford_in<A: Allocator + Copy>(
        bytes: &[u8],
        alloc: A,
    ) -> Result<String<A>, AllocFailure> {
        encode_with_alphabet_in(bytes, CROCKFORD_ALPHABET, false, alloc)
    }

    /// Decode Douglas Crockford's Base32.
    ///
    /// Ignores `-` separator hyphens, accepts both cases, maps `I`/`L` -> `1` and `O` -> `0`, and
    /// strictly rejects invalid symbols (including `U`), invalid lengths, and non-zero trailing
    /// bits.
    pub fn decode_crockford_in<A: Allocator + Copy>(
        input: &str,
        alloc: A,
    ) -> Result<Vec<u8, A>, Base32Error> {
        let max_out = (input.len() / 8).saturating_mul(5).saturating_add(5);
        let mut out = Vec::try_with_capacity_in(max_out, alloc)?;

        let mut vals = [0u8; 8];
        let mut count = 0usize;
        let mut total_syms = 0usize;
        let mut last_offset = 0usize;

        for (offset, &b) in input.as_bytes().iter().enumerate() {
            if b == b'-' {
                continue;
            }
            let v = decode_crockford_byte(b, offset)?;
            if let Some(slot) = vals.get_mut(count) {
                *slot = v;
            }
            count += 1;
            total_syms += 1;
            last_offset = offset;
            if count == 8 {
                let decoded = decode_full_block(vals);
                out.try_extend_from_slice(&decoded)?;
                vals = [0u8; 8];
                count = 0;
            }
        }

        if count > 0 {
            push_tail_block(vals, count, last_offset, total_syms, &mut out)?;
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// RFC 7468 PEM Armor
// ---------------------------------------------------------------------------

/// RFC 7468 PEM textual encoding and decoding over `blaze_base64::STANDARD`.
pub mod pem {
    use core::alloc::Allocator;
    use core::fmt;

    use blaze_base64::{DecodeError as Base64DecodeError, STANDARD};
    use blaze_own::{AllocFailure, String, Vec};

    /// One decoded RFC 7468 PEM encapsulation block.
    pub struct PemBlock<A: Allocator> {
        /// Encapsulation label (e.g. `"CERTIFICATE"`, `"PRIVATE KEY"`).
        pub label: String<A>,
        /// Decoded DER payload bytes.
        pub der: Vec<u8, A>,
    }

    impl<A: Allocator> fmt::Debug for PemBlock<A> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("PemBlock")
                .field("label", &self.label.as_str())
                .field("der_len", &self.der.len())
                .finish()
        }
    }

    impl<A: Allocator, B: Allocator> PartialEq<PemBlock<B>> for PemBlock<A> {
        fn eq(&self, other: &PemBlock<B>) -> bool {
            self.label.as_str() == other.label.as_str() && self.der.as_slice() == other.der.as_slice()
        }
    }

    impl<A: Allocator> Eq for PemBlock<A> {}

    /// Why RFC 7468 PEM parsing or encoding failed.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum PemError {
        /// Input length exceeded the caller's `max_bytes` bound.
        InputTooLarge { bytes: usize, limit: usize },
        /// PEM label is empty or violates the RFC 7468 §3 label grammar.
        InvalidLabel,
        /// Encountered `-----END ...-----` outside an open PEM block.
        UnexpectedEnd { line: usize },
        /// Closing `-----END ...-----` label did not match the opening `-----BEGIN ...-----`.
        MismatchedEnd { line: usize },
        /// Opening `-----BEGIN ...-----` had no matching `-----END ...-----`.
        UnterminatedBlock { start_line: usize },
        /// Base64 body inside a PEM block failed strict RFC 4648 decoding.
        InvalidBase64 { line: usize, error: Base64DecodeError },
        /// Formatting destination returned [`fmt::Error`].
        Write(fmt::Error),
        /// Backing allocator refused memory.
        Alloc(AllocFailure),
    }

    impl From<AllocFailure> for PemError {
        fn from(err: AllocFailure) -> Self {
            PemError::Alloc(err)
        }
    }

    impl From<fmt::Error> for PemError {
        fn from(err: fmt::Error) -> Self {
            PemError::Write(err)
        }
    }

    impl fmt::Display for PemError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match *self {
                PemError::InputTooLarge { bytes, limit } => {
                    write!(f, "bstd::otp::pem: input size {bytes} bytes exceeds limit {limit}")
                }
                PemError::InvalidLabel => {
                    f.write_str("bstd::otp::pem: label violates RFC 7468 label charset")
                }
                PemError::UnexpectedEnd { line } => {
                    write!(f, "bstd::otp::pem: unexpected END boundary at line {line}")
                }
                PemError::MismatchedEnd { line } => {
                    write!(f, "bstd::otp::pem: END label does not match BEGIN label at line {line}")
                }
                PemError::UnterminatedBlock { start_line } => {
                    write!(f, "bstd::otp::pem: unterminated BEGIN block opened at line {start_line}")
                }
                PemError::InvalidBase64 { line, error } => {
                    write!(f, "bstd::otp::pem: invalid base64 body ending at line {line} ({error})")
                }
                PemError::Write(_) => f.write_str("bstd::otp::pem: output formatter failed"),
                PemError::Alloc(err) => write!(f, "bstd::otp::pem: allocation failed ({err})"),
            }
        }
    }

    impl core::error::Error for PemError {}

    /// RFC 7468 §3 `labelchar = %x21-2C / %x2E-7E` (printable ASCII excluding space and `-`).
    #[inline]
    const fn is_label_char(b: u8) -> bool {
        b >= 0x21 && b <= 0x7e && b != b'-'
    }

    /// Validate a non-empty RFC 7468 label: `labelchar *( ["-" / SP] labelchar )`.
    #[must_use]
    pub fn is_valid_label(label: &str) -> bool {
        let bytes = label.as_bytes();
        let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
            return false;
        };
        if !is_label_char(first) || !is_label_char(last) {
            return false;
        }
        let mut prev_sep = false;
        for &b in bytes {
            if is_label_char(b) {
                prev_sep = false;
            } else if b == b' ' || b == b'-' {
                if prev_sep {
                    return false;
                }
                prev_sep = true;
            } else {
                return false;
            }
        }
        true
    }

    #[inline]
    fn parse_begin_label(line: &str) -> Option<&str> {
        line.strip_prefix("-----BEGIN ")?.strip_suffix("-----")
    }

    #[inline]
    fn parse_end_label(line: &str) -> Option<&str> {
        line.strip_prefix("-----END ")?.strip_suffix("-----")
    }

    /// Parse all RFC 7468 PEM blocks from `input`, enforcing `max_bytes`.
    ///
    /// Text outside `-----BEGIN LABEL-----` / `-----END LABEL-----` boundaries is ignored per
    /// RFC 7468 §2, except stray `-----END ...-----` lines which are rejected. CRLF/LF and ASCII
    /// horizontal whitespace inside the Base64 body are stripped before strict decoding via
    /// `blaze_base64::STANDARD`.
    pub fn parse_pem_in<A: Allocator + Copy>(
        input: &str,
        max_bytes: usize,
        alloc: A,
    ) -> Result<Vec<PemBlock<A>, A>, PemError> {
        if input.len() > max_bytes {
            return Err(PemError::InputTooLarge { bytes: input.len(), limit: max_bytes });
        }

        let mut blocks = Vec::new_in(alloc);
        let mut current: Option<(String<A>, usize)> = None;
        let mut b64_buf: Vec<u8, A> = Vec::new_in(alloc);

        for (zero_idx, raw_line) in input.split('\n').enumerate() {
            let line_no = zero_idx.saturating_add(1);
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

            if let Some((open_label, start_line)) = current.take() {
                if let Some(end_label) = parse_end_label(line) {
                    if end_label != open_label.as_str() {
                        return Err(PemError::MismatchedEnd { line: line_no });
                    }
                    let max_dec = STANDARD.decoded_len_most(b64_buf.len());
                    let mut der = Vec::try_with_capacity_in(max_dec, alloc)?;
                    for _ in 0..max_dec {
                        der.try_push(0u8).map_err(|r| PemError::Alloc(r.why))?;
                    }
                    let n = STANDARD
                        .decode_into(b64_buf.as_slice(), der.as_mut_slice())
                        .map_err(|error| PemError::InvalidBase64 { line: line_no, error })?;
                    der.truncate(n);
                    blocks
                        .try_push(PemBlock { label: open_label, der })
                        .map_err(|r| PemError::Alloc(r.why))?;
                    b64_buf.clear();
                } else if parse_begin_label(line).is_some() {
                    return Err(PemError::UnterminatedBlock { start_line });
                } else {
                    for &b in line.as_bytes() {
                        if !matches!(b, b' ' | b'\t' | b'\r') {
                            b64_buf.try_push(b).map_err(|r| PemError::Alloc(r.why))?;
                        }
                    }
                    current = Some((open_label, start_line));
                }
            } else if let Some(begin_label) = parse_begin_label(line) {
                if !is_valid_label(begin_label) {
                    return Err(PemError::InvalidLabel);
                }
                let owned_label = String::try_from_str_in(begin_label, alloc)?;
                current = Some((owned_label, line_no));
                b64_buf.clear();
            } else if parse_end_label(line).is_some() {
                return Err(PemError::UnexpectedEnd { line: line_no });
            }
        }

        if let Some((_, start_line)) = current {
            return Err(PemError::UnterminatedBlock { start_line });
        }

        Ok(blocks)
    }

    /// Encode `der` with RFC 7468 PEM armor wrapped at 64 Base64 characters per line into `out`.
    pub fn encode_pem_into<W: fmt::Write>(
        label: &str,
        der: &[u8],
        out: &mut W,
    ) -> Result<(), PemError> {
        if !is_valid_label(label) {
            return Err(PemError::InvalidLabel);
        }
        out.write_str("-----BEGIN ")?;
        out.write_str(label)?;
        out.write_str("-----\n")?;

        // 48 raw bytes encode to exactly 64 Base64 characters (RFC 7468 §2).
        let mut line_buf = [0u8; 64];
        for chunk in der.chunks(48) {
            let Ok(n) = STANDARD.encode_into(chunk, &mut line_buf) else {
                return Err(PemError::Write(fmt::Error));
            };
            if let Some(encoded_bytes) = line_buf.get(..n)
                && let Ok(text) = core::str::from_utf8(encoded_bytes)
            {
                out.write_str(text)?;
                out.write_char('\n')?;
            }
        }

        out.write_str("-----END ")?;
        out.write_str(label)?;
        out.write_str("-----\n")?;
        Ok(())
    }

    /// Encode `der` with RFC 7468 PEM armor into a newly allocated [`String`] in `alloc`.
    pub fn encode_pem_in<A: Allocator + Copy>(
        label: &str,
        der: &[u8],
        alloc: A,
    ) -> Result<String<A>, PemError> {
        let mut out = String::new_in(alloc);
        let mut writer = super::TryStringWriter { out: &mut out, err: None };
        match encode_pem_into(label, der, &mut writer) {
            Ok(()) => Ok(out),
            Err(err) => {
                if let Some(alloc_err) = writer.err {
                    Err(PemError::Alloc(alloc_err))
                } else {
                    Err(err)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal FIPS 180-4 SHA-1 & RFC 2104 HMAC-SHA1 (for RFC 4226 / RFC 6238)
// ---------------------------------------------------------------------------

const SHA1_BLOCK: usize = 64;
const SHA1_LEN: usize = 20;

struct Sha1 {
    state: [u32; 5],
    buf: [u8; SHA1_BLOCK],
    buf_len: usize,
    total_bytes: u64,
}

impl Sha1 {
    const fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            buf: [0u8; SHA1_BLOCK],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    fn compress_block(state: &mut [u32; 5], block: &[u8]) {
        let mut w = [0u32; 80];
        for (slot, word_bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
            if let [b0, b1, b2, b3] = *word_bytes {
                *slot = u32::from_be_bytes([b0, b1, b2, b3]);
            }
        }
        let mut i = 16usize;
        while i < 80 {
            let x = w.get(i - 3).copied().unwrap_or(0)
                ^ w.get(i - 8).copied().unwrap_or(0)
                ^ w.get(i - 14).copied().unwrap_or(0)
                ^ w.get(i - 16).copied().unwrap_or(0);
            if let Some(slot) = w.get_mut(i) {
                *slot = x.rotate_left(1);
            }
            i += 1;
        }

        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for (round, &wi) in w.iter().enumerate() {
            let (f, k) = match round {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc_u32),
                _ => (b ^ c ^ d, 0xca62_c1d6_u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        let [h0, h1, h2, h3, h4] = *state;
        *state = [
            h0.wrapping_add(a),
            h1.wrapping_add(b),
            h2.wrapping_add(c),
            h3.wrapping_add(d),
            h4.wrapping_add(e),
        ];
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self
            .total_bytes
            .wrapping_add(u64::try_from(data.len()).unwrap_or(u64::MAX));

        if self.buf_len > 0 {
            let take = (SHA1_BLOCK - self.buf_len).min(data.len());
            if let (Some((head, rest)), Some(dst)) = (
                data.split_at_checked(take),
                self.buf.get_mut(self.buf_len..self.buf_len + take),
            ) {
                dst.copy_from_slice(head);
                self.buf_len += take;
                data = rest;
            }
            if self.buf_len == SHA1_BLOCK {
                Self::compress_block(&mut self.state, &self.buf);
                self.buf_len = 0;
            }
        }

        let mut chunks = data.chunks_exact(SHA1_BLOCK);
        for chunk in chunks.by_ref() {
            Self::compress_block(&mut self.state, chunk);
        }

        let rem = chunks.remainder();
        if !rem.is_empty()
            && let Some(dst) = self.buf.get_mut(..rem.len())
        {
            dst.copy_from_slice(rem);
            self.buf_len = rem.len();
        }
    }

    fn finish(mut self) -> [u8; SHA1_LEN] {
        let bit_len = self.total_bytes.wrapping_mul(8);
        if let Some(slot) = self.buf.get_mut(self.buf_len) {
            *slot = 0x80;
            self.buf_len += 1;
        }
        if self.buf_len > 56 {
            if let Some(pad) = self.buf.get_mut(self.buf_len..SHA1_BLOCK) {
                pad.fill(0);
            }
            Self::compress_block(&mut self.state, &self.buf);
            self.buf_len = 0;
        }
        if let Some(pad) = self.buf.get_mut(self.buf_len..56) {
            pad.fill(0);
        }
        if let Some(len_dst) = self.buf.get_mut(56..SHA1_BLOCK) {
            len_dst.copy_from_slice(&bit_len.to_be_bytes());
        }
        Self::compress_block(&mut self.state, &self.buf);

        let mut out = [0u8; SHA1_LEN];
        for (dst, &word) in out.chunks_exact_mut(4).zip(self.state.iter()) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn sha1(data: &[u8]) -> [u8; SHA1_LEN] {
    let mut h = Sha1::new();
    h.update(data);
    h.finish()
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; SHA1_LEN] {
    let mut block = [0u8; SHA1_BLOCK];
    if key.len() > SHA1_BLOCK {
        let hashed = sha1(key);
        if let Some(dst) = block.get_mut(..SHA1_LEN) {
            dst.copy_from_slice(&hashed);
        }
    } else if let Some(dst) = block.get_mut(..key.len()) {
        dst.copy_from_slice(key);
    }

    let mut ipad = [0u8; SHA1_BLOCK];
    let mut opad = [0u8; SHA1_BLOCK];
    for ((ip, op), &b) in ipad.iter_mut().zip(opad.iter_mut()).zip(block.iter()) {
        *ip = b ^ 0x36;
        *op = b ^ 0x5c;
    }

    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_hash = inner.finish();

    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finish()
}

// ---------------------------------------------------------------------------
// RFC 4226 HOTP & RFC 6238 TOTP
// ---------------------------------------------------------------------------

/// Hash algorithm used for HMAC in HOTP (RFC 4226) and TOTP (RFC 6238).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OtpAlgorithm {
    /// HMAC-SHA1 (RFC 4226 & RFC 6238 default).
    Sha1,
    /// HMAC-SHA256 (RFC 6238 §1.2).
    Sha256,
}

impl OtpAlgorithm {
    /// Algorithm name for `otpauth://` URIs.
    #[must_use]
    pub const fn uri_name(self) -> &'static str {
        match self {
            OtpAlgorithm::Sha1 => "SHA1",
            OtpAlgorithm::Sha256 => "SHA256",
        }
    }
}

/// Configuration for RFC 6238 TOTP generation, verification, and `otpauth://` URIs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TotpConfig {
    /// HMAC digest algorithm.
    pub algorithm: OtpAlgorithm,
    /// Decimal output digits (`6..=8`).
    pub digits: u8,
    /// Time step duration in seconds (must be `> 0`).
    pub period_secs: u64,
}

impl TotpConfig {
    /// Standard 6-digit, 30-second SHA-1 TOTP configuration.
    pub const SHA1_6_30: Self = Self {
        algorithm: OtpAlgorithm::Sha1,
        digits: 6,
        period_secs: 30,
    };

    /// Standard 6-digit, 30-second SHA-256 TOTP configuration.
    pub const SHA256_6_30: Self = Self {
        algorithm: OtpAlgorithm::Sha256,
        digits: 6,
        period_secs: 30,
    };

    const fn validate(self) -> Result<(), OtpError> {
        if self.digits < 6 || self.digits > 8 {
            return Err(OtpError::InvalidDigits { digits: self.digits });
        }
        if self.period_secs == 0 {
            return Err(OtpError::ZeroPeriod);
        }
        Ok(())
    }
}

/// Why an HOTP, TOTP, or `otpauth://` URI operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OtpError {
    /// Shared secret slice was empty.
    EmptySecret,
    /// Digit count was outside the supported range `6..=8`.
    InvalidDigits { digits: u8 },
    /// TOTP `period_secs` was zero.
    ZeroPeriod,
    /// Requested `skew_steps` exceeded [`MAX_SKEW_STEPS`].
    SkewTooLarge { skew_steps: u64, max: u64 },
    /// Backing allocator refused memory.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for OtpError {
    fn from(err: AllocFailure) -> Self {
        OtpError::Alloc(err)
    }
}

impl fmt::Display for OtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            OtpError::EmptySecret => f.write_str("bstd::otp: shared secret must not be empty"),
            OtpError::InvalidDigits { digits } => {
                write!(f, "bstd::otp: digit count {digits} is outside 6..=8")
            }
            OtpError::ZeroPeriod => f.write_str("bstd::otp: TOTP period_secs must be greater than zero"),
            OtpError::SkewTooLarge { skew_steps, max } => {
                write!(f, "bstd::otp: skew_steps {skew_steps} exceeds maximum {max}")
            }
            OtpError::Alloc(err) => write!(f, "bstd::otp: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for OtpError {}

#[inline]
const fn digits_modulus(digits: u8) -> Result<u32, OtpError> {
    match digits {
        6 => Ok(1_000_000),
        7 => Ok(10_000_000),
        8 => Ok(100_000_000),
        _ => Err(OtpError::InvalidDigits { digits }),
    }
}

fn dynamic_truncate(mac: &[u8], digits: u8, plant_missing_msb_mask: bool) -> Result<u32, OtpError> {
    let modulus = digits_modulus(digits)?;
    let last = mac.last().copied().unwrap_or(0);
    let offset = usize::from(last & 0x0f);
    let [b0, b1, b2, b3] = match mac.get(offset..offset + 4) {
        Some(&[x0, x1, x2, x3]) => [x0, x1, x2, x3],
        _ => [0, 0, 0, 0],
    };
    let top = if plant_missing_msb_mask { b0 } else { b0 & 0x7f };
    let bin_code = u32::from_be_bytes([top, b1, b2, b3]);
    Ok(bin_code % modulus)
}

fn hotp_planted(
    secret: &[u8],
    counter: u64,
    algorithm: OtpAlgorithm,
    digits: u8,
    plant_missing_msb_mask: bool,
) -> Result<u32, OtpError> {
    if secret.is_empty() {
        return Err(OtpError::EmptySecret);
    }
    let counter_bytes = counter.to_be_bytes();
    match algorithm {
        OtpAlgorithm::Sha1 => {
            let mac = hmac_sha1(secret, &counter_bytes);
            dynamic_truncate(&mac, digits, plant_missing_msb_mask)
        }
        OtpAlgorithm::Sha256 => {
            let mac = hmac_sha256(secret, &counter_bytes);
            dynamic_truncate(&mac, digits, plant_missing_msb_mask)
        }
    }
}

/// Compute an RFC 4226 HOTP code (`digits` in `6..=8`) for `counter` under `secret`.
pub fn hotp(
    secret: &[u8],
    counter: u64,
    algorithm: OtpAlgorithm,
    digits: u8,
) -> Result<u32, OtpError> {
    hotp_planted(secret, counter, algorithm, digits, false)
}

/// Compute an RFC 6238 TOTP code at explicit Unix timestamp `unix_secs` (zero ambient clock).
pub fn totp(secret: &[u8], unix_secs: u64, config: TotpConfig) -> Result<u32, OtpError> {
    config.validate()?;
    let step = unix_secs / config.period_secs;
    hotp(secret, step, config.algorithm, config.digits)
}

fn format_otp_digits(mut code: u32, digits: u8) -> [u8; 8] {
    let mut buf = [b'0'; 8];
    let mut i = usize::from(digits);
    while i > 0 {
        i -= 1;
        let digit = u8::try_from(code % 10).unwrap_or(0);
        if let Some(slot) = buf.get_mut(i) {
            *slot = b'0' + digit;
        }
        code /= 10;
    }
    buf
}

/// Verify a decimal TOTP `code` against `secret` at explicit timestamp `unix_secs` within
/// `step.saturating_sub(skew_steps)..=step.saturating_add(skew_steps)`.
///
/// Formats each candidate code zero-padded to `config.digits` and compares in constant time with
/// `crate::digest::ct_eq` across the entire window without short-circuiting.
pub fn verify_totp(
    secret: &[u8],
    code: &str,
    unix_secs: u64,
    skew_steps: u64,
    config: TotpConfig,
) -> Result<bool, OtpError> {
    config.validate()?;
    if secret.is_empty() {
        return Err(OtpError::EmptySecret);
    }
    if skew_steps > MAX_SKEW_STEPS {
        return Err(OtpError::SkewTooLarge { skew_steps, max: MAX_SKEW_STEPS });
    }
    let expected_len = usize::from(config.digits);
    if code.len() != expected_len {
        return Ok(false);
    }

    let step = unix_secs / config.period_secs;
    let start = step.saturating_sub(skew_steps);
    let end = step.saturating_add(skew_steps);
    let code_bytes = code.as_bytes();

    let mut matched = 0u8;
    let mut current = start;
    loop {
        let candidate = hotp(secret, current, config.algorithm, config.digits)?;
        let formatted = format_otp_digits(candidate, config.digits);
        let expected_slice = formatted.get(..expected_len).unwrap_or(&[]);
        if ct_eq(code_bytes, expected_slice) {
            matched |= 1;
        }
        if current == end {
            break;
        }
        current += 1;
    }

    Ok(core::hint::black_box(matched) != 0)
}

/// Format an `otpauth://totp/...` provisioning URI in `alloc`.
pub fn format_otpauth_uri_in<A: Allocator + Copy>(
    issuer: &str,
    account: &str,
    secret: &[u8],
    config: TotpConfig,
    alloc: A,
) -> Result<String<A>, OtpError> {
    config.validate()?;
    if secret.is_empty() {
        return Err(OtpError::EmptySecret);
    }
    let encoded_secret = base32::encode_rfc4648_in(secret, false, alloc)?;
    let mut out = String::new_in(alloc);
    let mut writer = TryStringWriter { out: &mut out, err: None };

    let write_res = (|| -> fmt::Result {
        writer.write_str("otpauth://totp/")?;
        if !issuer.is_empty() {
            percent_encode_into(issuer.as_bytes(), &mut writer)?;
            writer.write_char(':')?;
        }
        percent_encode_into(account.as_bytes(), &mut writer)?;
        writer.write_str("?secret=")?;
        writer.write_str(encoded_secret.as_str())?;
        if !issuer.is_empty() {
            writer.write_str("&issuer=")?;
            percent_encode_into(issuer.as_bytes(), &mut writer)?;
        }
        write!(
            writer,
            "&algorithm={}&digits={}&period={}",
            config.algorithm.uri_name(),
            config.digits,
            config.period_secs
        )
    })();

    if let Some(alloc_err) = writer.err {
        return Err(OtpError::Alloc(alloc_err));
    }
    let _ = write_res;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::*;

    #[test]
    fn rfc4226_appendix_d_hotp_vectors() {
        let secret = b"12345678901234567890";
        let expected: [u32; 10] = [
            755_224, 287_082, 359_152, 969_429, 338_314, 254_676, 287_922, 162_583, 399_871,
            520_489,
        ];
        for (counter, &want) in expected.iter().enumerate() {
            let ctr = u64::try_from(counter).unwrap_or(0);
            let got = hotp(secret, ctr, OtpAlgorithm::Sha1, 6);
            assert_eq!(got, Ok(want), "RFC 4226 counter {counter}");
        }
    }

    #[test]
    fn rfc6238_appendix_b_totp_vectors() {
        let sha1_secret = b"12345678901234567890";
        let sha256_secret = b"12345678901234567890123456789012";
        let cfg_sha1 = TotpConfig {
            algorithm: OtpAlgorithm::Sha1,
            digits: 8,
            period_secs: 30,
        };
        let cfg_sha256 = TotpConfig {
            algorithm: OtpAlgorithm::Sha256,
            digits: 8,
            period_secs: 30,
        };

        let cases: [(u64, u32, u32); 6] = [
            (59, 94_287_082, 46_119_246),
            (1_111_111_109, 7_081_804, 68_084_774),
            (1_111_111_111, 14_050_471, 67_062_674),
            (1_234_567_890, 89_005_924, 91_819_424),
            (2_000_000_000, 69_279_037, 90_698_825),
            (20_000_000_000, 65_353_130, 77_737_706),
        ];

        for (t, want_sha1, want_sha256) in cases {
            assert_eq!(totp(sha1_secret, t, cfg_sha1), Ok(want_sha1), "SHA-1 at t={t}");
            assert_eq!(totp(sha256_secret, t, cfg_sha256), Ok(want_sha256), "SHA-256 at t={t}");
        }

        // Verify leading-zero formatting ("07081804") at t = 1111111109.
        assert_eq!(verify_totp(sha1_secret, "07081804", 1_111_111_109, 0, cfg_sha1), Ok(true));
        assert_eq!(verify_totp(sha1_secret, "7081804", 1_111_111_109, 0, cfg_sha1), Ok(false));
    }

    #[test]
    fn totp_skew_window_and_validation() {
        let h = heap();
        let secret = b"12345678901234567890";
        let cfg = TotpConfig::SHA1_6_30;
        let base_t = 3_000u64; // step 100
        let code_prev = totp(secret, base_t - 30, cfg).unwrap_or(0);
        let code_curr = totp(secret, base_t, cfg).unwrap_or(0);
        let code_next = totp(secret, base_t + 30, cfg).unwrap_or(0);
        let code_far = totp(secret, base_t + 60, cfg).unwrap_or(0);

        let s_prev = format_in!(h.alloc(), "{code_prev:06}");
        let s_curr = format_in!(h.alloc(), "{code_curr:06}");
        let s_next = format_in!(h.alloc(), "{code_next:06}");
        let s_far = format_in!(h.alloc(), "{code_far:06}");

        assert_eq!(verify_totp(secret, s_curr.as_str(), base_t, 0, cfg), Ok(true));
        assert_eq!(verify_totp(secret, s_prev.as_str(), base_t, 0, cfg), Ok(false));
        assert_eq!(verify_totp(secret, s_prev.as_str(), base_t, 1, cfg), Ok(true));
        assert_eq!(verify_totp(secret, s_next.as_str(), base_t, 1, cfg), Ok(true));
        assert_eq!(verify_totp(secret, s_far.as_str(), base_t, 1, cfg), Ok(false));
        assert_eq!(verify_totp(secret, s_far.as_str(), base_t, 2, cfg), Ok(true));

        assert_eq!(hotp(b"", 0, OtpAlgorithm::Sha1, 6), Err(OtpError::EmptySecret));
        assert_eq!(hotp(secret, 0, OtpAlgorithm::Sha1, 5), Err(OtpError::InvalidDigits { digits: 5 }));
        assert_eq!(hotp(secret, 0, OtpAlgorithm::Sha1, 9), Err(OtpError::InvalidDigits { digits: 9 }));
        assert_eq!(
            totp(
                secret,
                0,
                TotpConfig { algorithm: OtpAlgorithm::Sha1, digits: 6, period_secs: 0 }
            ),
            Err(OtpError::ZeroPeriod)
        );
        assert_eq!(
            verify_totp(secret, "000000", 0, MAX_SKEW_STEPS + 1, cfg),
            Err(OtpError::SkewTooLarge { skew_steps: MAX_SKEW_STEPS + 1, max: MAX_SKEW_STEPS })
        );
    }

    #[test]
    fn otpauth_uri_formatting() {
        let h = heap();
        let alloc = h.alloc();
        let secret = b"12345678901234567890";
        let uri = format_otpauth_uri_in(
            "Acme Corp",
            "alice@example.com",
            secret,
            TotpConfig::SHA256_6_30,
            alloc,
        );
        assert!(uri.is_ok());
        if let Ok(u) = uri {
            assert_eq!(
                u.as_str(),
                "otpauth://totp/Acme%20Corp:alice%40example.com?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Acme%20Corp&algorithm=SHA256&digits=6&period=30"
            );
        }

        let no_issuer = format_otpauth_uri_in("", "bob", b"foo", TotpConfig::SHA1_6_30, alloc);
        assert!(no_issuer.is_ok());
        if let Ok(u) = no_issuer {
            assert_eq!(u.as_str(), "otpauth://totp/bob?secret=MZXW6&algorithm=SHA1&digits=6&period=30");
        }
    }

    #[test]
    fn base32_rfc4648_section10_and_crockford() {
        let h = heap();
        let alloc = h.alloc();
        let vectors: [(&[u8], &str, &str); 7] = [
            (b"", "", ""),
            (b"f", "MY======", "MY"),
            (b"fo", "MZXQ====", "MZXQ"),
            (b"foo", "MZXW6===", "MZXW6"),
            (b"foob", "MZXW6YQ=", "MZXW6YQ"),
            (b"fooba", "MZXW6YTB", "MZXW6YTB"),
            (b"foobar", "MZXW6YTBOI======", "MZXW6YTBOI"),
        ];

        for (raw, padded, unpadded) in vectors {
            let enc_pad = base32::encode_rfc4648_in(raw, true, alloc);
            let enc_nopad = base32::encode_rfc4648_in(raw, false, alloc);
            assert_eq!(enc_pad.as_ref().map(OwnString::as_str), Ok(padded));
            assert_eq!(enc_nopad.as_ref().map(OwnString::as_str), Ok(unpadded));

            let dec_pad = base32::decode_rfc4648_in(padded, alloc);
            let dec_nopad = base32::decode_rfc4648_in(unpadded, alloc);
            assert_eq!(dec_pad.as_ref().map(OwnVec::as_slice), Ok(raw));
            assert_eq!(dec_nopad.as_ref().map(OwnVec::as_slice), Ok(raw));
        }

        // Case-insensitive RFC 4648 decode.
        let lower = base32::decode_rfc4648_in("mzxw6ytboi======", alloc);
        assert_eq!(lower.as_ref().map(OwnVec::as_slice), Ok(b"foobar".as_slice()));

        // Strict rejections: invalid char, misplaced/wrong padding, non-zero trailing bits, invalid length.
        assert!(matches!(
            base32::decode_rfc4648_in("MZXW61TB", alloc),
            Err(base32::Base32Error::InvalidByte { offset: 5, byte: b'1' })
        ));
        assert!(matches!(
            base32::decode_rfc4648_in("MZXW61==", alloc),
            Err(base32::Base32Error::InvalidPadding { offset: 6 })
        ));
        assert!(matches!(
            base32::decode_rfc4648_in("MY=A====", alloc),
            Err(base32::Base32Error::InvalidPadding { offset: 3 })
        ));
        assert!(matches!(
            base32::decode_rfc4648_in("MZ======", alloc),
            Err(base32::Base32Error::NonZeroTrailingBits { offset: 1 })
        ));
        assert!(matches!(
            base32::decode_rfc4648_in("M", alloc),
            Err(base32::Base32Error::InvalidLength { len: 1 })
        ));

        // Crockford Base32: hyphens and ambiguous I/L/O mapping.
        let c_enc = base32::encode_crockford_in(b"foobar", alloc);
        assert_eq!(c_enc.as_ref().map(OwnString::as_str), Ok("CSQPYRK1E8"));
        let c_dec = base32::decode_crockford_in("csqp-yrkl-e8", alloc);
        assert_eq!(c_dec.as_ref().map(OwnVec::as_slice), Ok(b"foobar".as_slice()));

        // Crockford maps O/o -> 0 and I/i/L/l -> 1, rejects U/u.
        let zero_one = base32::decode_crockford_in("Oo-IiLl", alloc);
        let canonical = base32::decode_crockford_in("001111", alloc);
        assert!(matches!(zero_one, Err(base32::Base32Error::InvalidLength { len: 6 })));
        assert_eq!(zero_one, canonical);

        let mapped = base32::decode_crockford_in("O-I-L-0-1-o-i-l", alloc);
        let direct = base32::decode_crockford_in("01101011", alloc);
        assert_eq!(mapped, direct);
        assert!(matches!(
            base32::decode_crockford_in("CSQPYRKU", alloc),
            Err(base32::Base32Error::InvalidByte { offset: 7, byte: b'U' })
        ));
    }

    #[test]
    fn pem_armor_round_trip_and_errors() {
        let h = heap();
        let alloc = h.alloc();
        let mut payload = OwnVec::new_in(alloc);
        for i in 0u8..120 {
            payload.push(i);
        }

        let armored = pem::encode_pem_in("CERTIFICATE", payload.as_slice(), alloc);
        assert!(armored.is_ok());
        if let Ok(text) = armored {
            // Check 64-char line wrapping on base64 body lines.
            for line in text.as_str().lines() {
                if !line.starts_with("-----") {
                    assert!(line.len() <= 64, "line exceeded 64 chars: {}", line.len());
                }
            }
            let parsed = pem::parse_pem_in(text.as_str(), 16_384, alloc);
            assert!(parsed.is_ok());
            if let Ok(blocks) = parsed {
                assert_eq!(blocks.len(), 1);
                if let Some(blk) = blocks.first() {
                    assert_eq!(blk.label.as_str(), "CERTIFICATE");
                    assert_eq!(blk.der.as_slice(), payload.as_slice());
                }
            }
        }

        // CRLF + explanatory text + multi-block.
        let multi = "Comment line\r\n-----BEGIN RSA PRIVATE KEY-----\r\nZm9v\r\n-----END RSA PRIVATE KEY-----\r\n\r\n-----BEGIN PUBLIC KEY-----\nYmFy\n-----END PUBLIC KEY-----\n";
        let blocks = pem::parse_pem_in(multi, 4096, alloc);
        assert!(blocks.is_ok());
        if let Ok(bs) = blocks {
            assert_eq!(bs.len(), 2);
            assert_eq!(bs.first().map(|b| b.der.as_slice()), Some(b"foo".as_slice()));
            assert_eq!(bs.get(1).map(|b| b.der.as_slice()), Some(b"bar".as_slice()));
        }

        // Strict error cases.
        assert!(matches!(
            pem::parse_pem_in("-----BEGIN A-----\nZm9v\n-----END B-----\n", 1024, alloc),
            Err(pem::PemError::MismatchedEnd { line: 3 })
        ));
        assert!(matches!(
            pem::parse_pem_in("-----BEGIN A-----\nZm9v\n", 1024, alloc),
            Err(pem::PemError::UnterminatedBlock { start_line: 1 })
        ));
        assert!(matches!(
            pem::parse_pem_in("-----END A-----\n", 1024, alloc),
            Err(pem::PemError::UnexpectedEnd { line: 1 })
        ));
        assert_eq!(pem::encode_pem_in("", b"hi", alloc), Err(pem::PemError::InvalidLabel));
        assert_eq!(pem::encode_pem_in("BAD--LABEL", b"hi", alloc), Err(pem::PemError::InvalidLabel));
        assert!(matches!(
            pem::parse_pem_in("-----BEGIN A-----\nZm9v\n-----END A-----\n", 8, alloc),
            Err(pem::PemError::InputTooLarge { .. })
        ));
    }

    fn draw_bytes<A: Allocator + Copy>(tape: &Tape, min_len: usize, max_len: usize, alloc: A) -> OwnVec<u8, A> {
        let len = tape.int_in(min_len..=max_len, "len");
        let mut buf = OwnVec::with_capacity_in(len, alloc);
        buf.resize(len, 0u8);
        tape.fill(buf.as_mut_slice(), "bytes");
        buf
    }

    /// The invariant: RFC 4648 and Crockford Base32 encoding and decoding round-trip arbitrary byte slices.
    #[test]
    fn property_base32_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let raw = draw_bytes(tape, 0, 64, alloc);
            let pad = tape.coin("pad");

            let rfc_enc = base32::encode_rfc4648_in(raw.as_slice(), pad, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "rfc encode: {e}").as_str().into()))?;
            let rfc_dec = base32::decode_rfc4648_in(rfc_enc.as_str(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "rfc decode: {e}").as_str().into()))?;
            if rfc_dec.as_slice() != raw.as_slice() {
                return Err(Broke::Oracle(
                    format_in!(alloc, "RFC 4648 Base32 round-trip mismatch").as_str().into(),
                ));
            }

            let crk_enc = base32::encode_crockford_in(raw.as_slice(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "crockford encode: {e}").as_str().into()))?;
            let crk_dec = base32::decode_crockford_in(crk_enc.as_str(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "crockford decode: {e}").as_str().into()))?;
            if crk_dec.as_slice() != raw.as_slice() {
                return Err(Broke::Oracle(
                    format_in!(alloc, "Crockford Base32 round-trip mismatch").as_str().into(),
                ));
            }
            Ok(())
        });
    }

    /// The invariant: RFC 7468 PEM armor encoding and parsing round-trip arbitrary DER payloads and valid labels.
    #[test]
    fn property_pem_round_trip() {
        let h = heap();
        const LABELS: &[&str] = &[
            "CERTIFICATE",
            "RSA PRIVATE KEY",
            "PUBLIC KEY",
            "X509 CRL",
            "BLAZE-TEST TOKEN",
        ];
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let label = tape.pick(LABELS, "label").copied().unwrap_or("CERTIFICATE");
            let der = draw_bytes(tape, 0, 160, alloc);

            let armored = pem::encode_pem_in(label, der.as_slice(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "pem encode: {e}").as_str().into()))?;
            let blocks = pem::parse_pem_in(armored.as_str(), 16_384, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "pem parse: {e}").as_str().into()))?;

            if blocks.len() != 1 {
                return Err(Broke::Oracle(
                    format_in!(alloc, "expected 1 block, got {}", blocks.len()).as_str().into(),
                ));
            }
            if let Some(blk) = blocks.first() {
                if blk.label.as_str() != label || blk.der.as_slice() != der.as_slice() {
                    return Err(Broke::Oracle(
                        format_in!(alloc, "PEM block content mismatch").as_str().into(),
                    ));
                }
            }
            Ok(())
        });
    }

    /// The invariant: TOTP codes generated by `totp` verify within any covering skew window and respect RFC 4226 dynamic truncation.
    #[test]
    fn property_totp_generate_and_verify() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let secret = draw_bytes(tape, 10, 40, alloc);
            let alg = if tape.coin("sha256") {
                OtpAlgorithm::Sha256
            } else {
                OtpAlgorithm::Sha1
            };
            let digits = tape.int_in(6u8..=8u8, "digits");
            let period_secs = tape.int_in(15u64..=60u64, "period");
            let skew = tape.int_in(0u64..=3u64, "skew");
            let step = tape.int_in(10u64..=100_000u64, "step");
            let cfg = TotpConfig { algorithm: alg, digits, period_secs };

            let gen_time = step * period_secs;
            let code = totp(secret.as_slice(), gen_time, cfg)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "totp: {e}").as_str().into()))?;
            let formatted = format_otp_digits(code, digits);
            let code_str = core::str::from_utf8(formatted.get(..usize::from(digits)).unwrap_or(&[]))
                .unwrap_or("");

            let delta_steps = tape.int_in(0u64..=skew, "delta_steps");
            let verify_time = if tape.coin("forward") {
                gen_time.saturating_add(delta_steps * period_secs)
            } else {
                gen_time.saturating_sub(delta_steps * period_secs)
            };

            let ok = verify_totp(secret.as_slice(), code_str, verify_time, skew, cfg)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "verify_totp: {e}").as_str().into()))?;
            if !ok {
                return Err(Broke::Oracle(
                    format_in!(alloc, "TOTP failed to verify within skew window").as_str().into(),
                ));
            }
            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_missing_msb_mask() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let secret = draw_bytes(tape, 16, 32, alloc);
            let counter = tape.int_in(0u64..=100_000u64, "counter");
            let alg = if tape.coin("sha256") {
                OtpAlgorithm::Sha256
            } else {
                OtpAlgorithm::Sha1
            };
            let digits = tape.int_in(6u8..=8u8, "digits");
            let cfg = TotpConfig { algorithm: alg, digits, period_secs: 30 };

            let code = hotp_planted(secret.as_slice(), counter, alg, digits, plant)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "hotp: {e}").as_str().into()))?;
            let formatted = format_otp_digits(code, digits);
            let code_str = core::str::from_utf8(formatted.get(..usize::from(digits)).unwrap_or(&[]))
                .unwrap_or("");

            let verified = verify_totp(secret.as_slice(), code_str, counter * 30, 0, cfg)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "verify: {e}").as_str().into()))?;
            if !verified {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "HOTP dynamic truncation code {code_str} failed RFC 4226 verification"
                    )
                    .as_str()
                    .into(),
                ));
            }
            Ok(())
        });
    }
}
