//! Certificates out of PEM text (RFC 7468): each `CERTIFICATE` block, its base64 decoded with
//! blaze-base64. A bundle's other blocks (a key, OpenSSL's `TRUSTED CERTIFICATE`) and text
//! between blocks are passed over; a broken certificate block is an error that names its
//! offset, never skipped.

use std::{alloc::Allocator, fmt};

use super::{MOST_DER, Over, Roots};

const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
const END: &[u8] = b"-----END CERTIFICATE-----";

/// Why a PEM text was refused, at byte `at` of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PemError {
    pub at: usize,
    pub why: PemWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PemWhy {
    /// A `BEGIN CERTIFICATE` line with no `END CERTIFICATE` after it.
    Unterminated,
    /// A block whose base64 is past what [`MOST_DER`] bytes encode to.
    TooLong,
    /// The block's base64 is not canonical base64.
    Base64(blaze_base64::DecodeError),
    /// A bound of the roots passed.
    Over(Over),
}

impl fmt::Display for PemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at byte {}: ", self.at)?;
        match self.why {
            PemWhy::Unterminated => f.write_str("a certificate with no END line"),
            PemWhy::TooLong => write!(f, "a certificate past {MOST_DER} bytes"),
            PemWhy::Base64(e) => write!(f, "{e}"),
            PemWhy::Over(Over::Roots) => write!(f, "more than {} roots", super::MOST_ROOTS),
            PemWhy::Over(Over::Der { len }) => write!(f, "a root of {len} bytes"),
        }
    }
}

/// The first index of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|it| it == needle).map(|at| at + from)
}

/// Adds each certificate of `text` to `out`; how many blocks it held (a root already held is
/// counted, not added twice). `scratch` holds one block's symbols and bytes at a time.
pub fn certificates<A: Allocator, S: Allocator>(text: &[u8], out: &mut Roots<A>, scratch: S) -> Result<u32, PemError> {
    // The base64 of MOST_DER bytes, padded.
    const MOST_SYMBOLS: usize = MOST_DER.div_ceil(3) * 4;
    let mut symbols: Vec<u8, &S> = Vec::new_in(&scratch);
    // On the heap: a fiber's stack is small.
    let mut der: Vec<u8, &S> = Vec::new_in(&scratch);
    der.resize(MOST_DER, 0);
    let mut found = 0u32;
    let mut at = 0usize;
    while let Some(begin) = find(text, BEGIN, at) {
        let body = begin + BEGIN.len();
        let Some(end) = find(text, END, body) else {
            return Err(PemError { at: begin, why: PemWhy::Unterminated });
        };
        symbols.clear();
        for &byte in &text[body..end] {
            if byte.is_ascii_whitespace() {
                continue;
            }
            if symbols.len() == MOST_SYMBOLS {
                return Err(PemError { at: begin, why: PemWhy::TooLong });
            }
            symbols.push(byte);
        }
        let len = blaze_base64::STANDARD
            .decode_into(&symbols, &mut der)
            .map_err(|e| PemError { at: begin, why: PemWhy::Base64(e) })?;
        out.add(&der[..len]).map_err(|over| PemError { at: begin, why: PemWhy::Over(over) })?;
        found += 1;
        at = end + END.len();
    }
    Ok(found)
}
