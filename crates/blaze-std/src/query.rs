//! Pure `#![no_std]` RFC 3986 percent-encoding, WHATWG `application/x-www-form-urlencoded`
//! query strings, and sans-IO RFC 7578 `multipart/form-data` parsing and formatting.
//!
//! - Percent-encoding ([`percent_encode_into`], [`percent_encode_in`], [`percent_decode_into`],
//!   [`percent_decode_utf8_in`]) leaves RFC 3986 §2.3 unreserved bytes (`A-Z a-z 0-9 - . _ ~`)
//!   literal and encodes all others as uppercase `%XX`; decoding strictly rejects truncated or
//!   non-hex escapes.
//! - Query strings ([`parse_query_in`], [`format_query_into`], [`format_query_in`]) enforce
//!   [`QueryLimits`], strip an optional single leading `?`, skip empty segments, map `+` <-> `' '`,
//!   and treat keys without `=` as having an empty value.
//! - Multipart ([`parse_multipart_in`], [`format_multipart_into`]) returns zero-copy [`Part`]
//!   slices into the input buffer, validates safe RFC 2046 boundaries, enforces
//!   [`MultipartLimits`], and refuses header injection and `\r\n--<boundary>` body collisions.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_memchr::memmem;
use blaze_own::{AllocFailure, String, Vec};

const MAX_BOUNDARY_LEN: usize = 70;
const CRLF_DASH_PREFIX_LEN: usize = 4;
const DELIM_BUF_LEN: usize = CRLF_DASH_PREFIX_LEN + MAX_BOUNDARY_LEN;

/// Bounds enforced while parsing untrusted query strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QueryLimits {
    /// Maximum input length in bytes.
    pub most_bytes: usize,
    /// Maximum number of parsed query parameters.
    pub most_params: usize,
}

impl QueryLimits {
    /// Generous default bounds (1 MiB, 4,096 parameters).
    pub const USUAL: Self = Self {
        most_bytes: 1 << 20,
        most_params: 4096,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Bounds enforced while parsing untrusted `multipart/form-data` payloads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MultipartLimits {
    /// Maximum multipart body length in bytes.
    pub most_bytes: usize,
    /// Maximum number of parts in one payload.
    pub most_parts: usize,
    /// Maximum number of header lines permitted in a single part.
    pub most_headers_per_part: usize,
}

impl MultipartLimits {
    /// Generous default bounds (16 MiB, 512 parts, 32 headers per part).
    pub const USUAL: Self = Self {
        most_bytes: 16 << 20,
        most_parts: 512,
        most_headers_per_part: 32,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// A decoded `application/x-www-form-urlencoded` key/value pair.
pub struct QueryParam<A: Allocator> {
    /// Decoded parameter name.
    pub key: String<A>,
    /// Decoded parameter value.
    pub value: String<A>,
}

impl<A: Allocator> fmt::Debug for QueryParam<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryParam")
            .field("key", &self.key.as_str())
            .field("value", &self.value.as_str())
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<QueryParam<B>> for QueryParam<A> {
    fn eq(&self, other: &QueryParam<B>) -> bool {
        self.key.as_str() == other.key.as_str() && self.value.as_str() == other.value.as_str()
    }
}

impl<A: Allocator> Eq for QueryParam<A> {}

/// A single zero-copy part of a `multipart/form-data` payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Part<'a> {
    /// Value of the `Content-Disposition` `name` parameter.
    pub name: &'a str,
    /// Optional value of the `Content-Disposition` `filename` parameter.
    pub filename: Option<&'a str>,
    /// Optional value of the `Content-Type` header.
    pub content_type: Option<&'a str>,
    /// Raw part payload bytes borrowed directly from the input buffer.
    pub body: &'a [u8],
}

/// Why percent-decoding, query parsing, or multipart parsing/formatting failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryError {
    /// Input length exceeded the configured byte limit.
    InputTooLarge { bytes: usize, limit: usize },
    /// Query string contained more parameters than [`QueryLimits::most_params`].
    TooManyParams { limit: usize },
    /// A `%` escape sequence ended before two hexadecimal digits.
    TruncatedPercent { offset: usize },
    /// A `%` escape sequence contained non-hexadecimal bytes.
    InvalidPercentHex { offset: usize },
    /// Percent-decoded bytes were not valid UTF-8.
    InvalidUtf8 { valid_up_to: usize },
    /// Multipart boundary was empty, longer than 70 bytes, or contained forbidden bytes.
    InvalidBoundary,
    /// Multipart payload did not contain the opening `--<boundary>` delimiter.
    MissingBoundary,
    /// Multipart payload ended before the closing `--<boundary>--` delimiter.
    MissingClosingBoundary,
    /// A `--<boundary>` delimiter was not followed by `\r\n` or `--`.
    MalformedBoundary { offset: usize },
    /// Multipart payload contained more parts than [`MultipartLimits::most_parts`].
    TooManyParts { limit: usize },
    /// A multipart part contained more headers than [`MultipartLimits::most_headers_per_part`].
    TooManyHeaders { limit: usize },
    /// Part headers ended without a terminating blank CRLF line.
    UnterminatedHeaders { offset: usize },
    /// A multipart header line or `Content-Disposition` parameter was malformed.
    InvalidHeader { offset: usize },
    /// A multipart part lacked a `Content-Disposition` `name` parameter.
    MissingPartName { offset: usize },
    /// A [`Part`] metadata field contained forbidden characters (such as CR, LF, NUL, or `"`).
    InvalidHeaderField { part_index: usize },
    /// A part body contained `\r\n--<boundary>`, which would corrupt multipart framing.
    BoundaryCollision { part_index: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            QueryError::InputTooLarge { bytes, limit } => {
                write!(f, "bstd::query: input size {bytes} bytes exceeds limit {limit}")
            }
            QueryError::TooManyParams { limit } => {
                write!(f, "bstd::query: parameter count exceeds limit {limit}")
            }
            QueryError::TruncatedPercent { offset } => {
                write!(f, "bstd::query: truncated percent-escape at byte {offset}")
            }
            QueryError::InvalidPercentHex { offset } => {
                write!(f, "bstd::query: invalid hex digits in percent-escape at byte {offset}")
            }
            QueryError::InvalidUtf8 { valid_up_to } => {
                write!(f, "bstd::query: decoded bytes are not valid UTF-8 (valid up to byte {valid_up_to})")
            }
            QueryError::InvalidBoundary => {
                f.write_str("bstd::query: multipart boundary must be 1..=70 safe ASCII bytes without CR/LF")
            }
            QueryError::MissingBoundary => {
                f.write_str("bstd::query: multipart payload is missing opening boundary delimiter")
            }
            QueryError::MissingClosingBoundary => {
                f.write_str("bstd::query: multipart payload is missing closing `--boundary--` delimiter")
            }
            QueryError::MalformedBoundary { offset } => {
                write!(f, "bstd::query: malformed multipart boundary delimiter at byte {offset}")
            }
            QueryError::TooManyParts { limit } => {
                write!(f, "bstd::query: multipart part count exceeds limit {limit}")
            }
            QueryError::TooManyHeaders { limit } => {
                write!(f, "bstd::query: multipart part header count exceeds limit {limit}")
            }
            QueryError::UnterminatedHeaders { offset } => {
                write!(f, "bstd::query: unterminated multipart part headers at byte {offset}")
            }
            QueryError::InvalidHeader { offset } => {
                write!(f, "bstd::query: malformed multipart header at byte {offset}")
            }
            QueryError::MissingPartName { offset } => {
                write!(f, "bstd::query: multipart part at byte {offset} is missing Content-Disposition name")
            }
            QueryError::InvalidHeaderField { part_index } => {
                write!(f, "bstd::query: multipart part {part_index} contains forbidden header characters")
            }
            QueryError::BoundaryCollision { part_index } => {
                write!(f, "bstd::query: multipart part {part_index} body contains boundary delimiter")
            }
            QueryError::Alloc(err) => write!(f, "bstd::query: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for QueryError {}

// ---------------------------------------------------------------------------
// RFC 3986 percent-encoding
// ---------------------------------------------------------------------------

#[inline]
const fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

#[inline]
const fn hex_upper_byte(nibble: u8) -> u8 {
    match nibble & 0x0F {
        0 => b'0',
        1 => b'1',
        2 => b'2',
        3 => b'3',
        4 => b'4',
        5 => b'5',
        6 => b'6',
        7 => b'7',
        8 => b'8',
        9 => b'9',
        10 => b'A',
        11 => b'B',
        12 => b'C',
        13 => b'D',
        14 => b'E',
        15..=u8::MAX => b'F',
    }
}

#[inline]
const fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[inline]
fn pct_triple(b: u8) -> [u8; 3] {
    [b'%', hex_upper_byte(b >> 4), hex_upper_byte(b & 0x0F)]
}

#[inline]
fn write_pct_into<W: fmt::Write>(b: u8, out: &mut W) -> fmt::Result {
    let triple = pct_triple(b);
    if let Ok(s) = core::str::from_utf8(&triple) {
        out.write_str(s)?;
    }
    Ok(())
}

/// Percent-encode `bytes` per RFC 3986 §2.1 & §2.3 into `out`.
///
/// Unreserved bytes (`A-Z`, `a-z`, `0-9`, `-`, `.`, `_`, `~`) are written verbatim; every other
/// byte is emitted as uppercase `%XX`.
pub fn percent_encode_into<W: fmt::Write>(bytes: &[u8], out: &mut W) -> fmt::Result {
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if is_unreserved(b) {
            i += 1;
            continue;
        }
        if i > seg_start
            && let Some(chunk) = bytes.get(seg_start..i)
            && let Ok(s) = core::str::from_utf8(chunk)
        {
            out.write_str(s)?;
        }
        write_pct_into(b, out)?;
        i += 1;
        seg_start = i;
    }
    if i > seg_start
        && let Some(chunk) = bytes.get(seg_start..i)
        && let Ok(s) = core::str::from_utf8(chunk)
    {
        out.write_str(s)?;
    }
    Ok(())
}

/// Percent-encode `bytes` per RFC 3986 §2.1 & §2.3 into a new [`String`] in `alloc`.
pub fn percent_encode_in<A: Allocator + Copy>(
    bytes: &[u8],
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut encoded_len = 0usize;
    for &b in bytes {
        encoded_len = encoded_len.saturating_add(if is_unreserved(b) { 1 } else { 3 });
    }
    let mut out = String::try_with_capacity_in(encoded_len, alloc)?;
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if is_unreserved(b) {
            i += 1;
            continue;
        }
        if i > seg_start
            && let Some(chunk) = bytes.get(seg_start..i)
            && let Ok(s) = core::str::from_utf8(chunk)
        {
            out.try_push_str(s)?;
        }
        let triple = pct_triple(b);
        if let Ok(s) = core::str::from_utf8(&triple) {
            out.try_push_str(s)?;
        }
        i += 1;
        seg_start = i;
    }
    if i > seg_start
        && let Some(chunk) = bytes.get(seg_start..i)
        && let Ok(s) = core::str::from_utf8(chunk)
    {
        out.try_push_str(s)?;
    }
    Ok(out)
}

fn percent_decode_impl<A: Allocator>(
    input: &str,
    base_offset: usize,
    plus_as_space: bool,
    out: &mut Vec<u8, A>,
) -> Result<(), QueryError> {
    let bytes = input.as_bytes();
    out.try_reserve(bytes.len()).map_err(QueryError::Alloc)?;
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            if i > seg_start
                && let Some(chunk) = bytes.get(seg_start..i)
            {
                out.try_extend_from_slice(chunk).map_err(QueryError::Alloc)?;
            }
            let Some(&hi_byte) = bytes.get(i + 1) else {
                return Err(QueryError::TruncatedPercent { offset: base_offset + i });
            };
            let Some(&lo_byte) = bytes.get(i + 2) else {
                return Err(QueryError::TruncatedPercent { offset: base_offset + i });
            };
            let (Some(hi), Some(lo)) = (decode_hex_nibble(hi_byte), decode_hex_nibble(lo_byte))
            else {
                return Err(QueryError::InvalidPercentHex { offset: base_offset + i });
            };
            let decoded = (hi << 4) | lo;
            out.try_push(decoded).map_err(|r| QueryError::Alloc(r.why))?;
            i += 3;
            seg_start = i;
            continue;
        }
        if plus_as_space && b == b'+' {
            if i > seg_start
                && let Some(chunk) = bytes.get(seg_start..i)
            {
                out.try_extend_from_slice(chunk).map_err(QueryError::Alloc)?;
            }
            out.try_push(b' ').map_err(|r| QueryError::Alloc(r.why))?;
            i += 1;
            seg_start = i;
            continue;
        }
        i += 1;
    }
    if i > seg_start
        && let Some(chunk) = bytes.get(seg_start..i)
    {
        out.try_extend_from_slice(chunk).map_err(QueryError::Alloc)?;
    }
    Ok(())
}

/// Strictly percent-decode `input` into `out`, leaving `+` literal.
///
/// Refuses truncated `%` escapes and non-hexadecimal `%XX` sequences.
pub fn percent_decode_into<A: Allocator>(
    input: &str,
    out: &mut Vec<u8, A>,
) -> Result<(), QueryError> {
    percent_decode_impl(input, 0, false, out)
}

/// Strictly percent-decode `input` and validate the resulting bytes as UTF-8 in `alloc`.
pub fn percent_decode_utf8_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, QueryError> {
    let mut buf = Vec::new_in(alloc);
    percent_decode_into(input, &mut buf)?;
    String::from_utf8(buf).map_err(|e| QueryError::InvalidUtf8 {
        valid_up_to: e.utf8_error().valid_up_to(),
    })
}

// ---------------------------------------------------------------------------
// WHATWG application/x-www-form-urlencoded query strings
// ---------------------------------------------------------------------------

fn decode_form_component_in<A: Allocator + Copy>(
    component: &str,
    base_offset: usize,
    plus_as_space: bool,
    alloc: A,
) -> Result<String<A>, QueryError> {
    let mut buf = Vec::new_in(alloc);
    percent_decode_impl(component, base_offset, plus_as_space, &mut buf)?;
    String::from_utf8(buf).map_err(|e| QueryError::InvalidUtf8 {
        valid_up_to: e.utf8_error().valid_up_to(),
    })
}

/// Parse an `application/x-www-form-urlencoded` query string into owned parameters in `alloc`.
///
/// Strips an optional single leading `?`, skips empty segments, maps `+` to `' '`, strictly
/// percent-decodes keys and values as UTF-8, and treats segments without `=` as having an empty
/// value.
pub fn parse_query_in<A: Allocator + Copy>(
    input: &str,
    limits: QueryLimits,
    alloc: A,
) -> Result<Vec<QueryParam<A>, A>, QueryError> {
    parse_query_in_planted(input, limits, alloc, false)
}

fn parse_query_in_planted<A: Allocator + Copy>(
    input: &str,
    limits: QueryLimits,
    alloc: A,
    plant_skip_plus_space: bool,
) -> Result<Vec<QueryParam<A>, A>, QueryError> {
    if input.len() > limits.most_bytes {
        return Err(QueryError::InputTooLarge {
            bytes: input.len(),
            limit: limits.most_bytes,
        });
    }

    let (body, mut offset) = match input.strip_prefix('?') {
        Some(rest) => (rest, 1usize),
        None => (input, 0usize),
    };

    let mut params = Vec::new_in(alloc);
    let plus_as_space = !plant_skip_plus_space;

    for segment in body.split('&') {
        let seg_offset = offset;
        offset = offset.saturating_add(segment.len()).saturating_add(1);
        if segment.is_empty() {
            continue;
        }
        if params.len() >= limits.most_params {
            return Err(QueryError::TooManyParams { limit: limits.most_params });
        }

        let (raw_key, raw_val, val_offset) = match segment.split_once('=') {
            Some((k, v)) => (k, v, seg_offset.saturating_add(k.len()).saturating_add(1)),
            None => (segment, "", seg_offset.saturating_add(segment.len())),
        };

        let key = decode_form_component_in(raw_key, seg_offset, plus_as_space, alloc)?;
        let value = decode_form_component_in(raw_val, val_offset, plus_as_space, alloc)?;
        params
            .try_push(QueryParam { key, value })
            .map_err(|r| QueryError::Alloc(r.why))?;
    }

    Ok(params)
}

fn form_encode_component_into<W: fmt::Write>(text: &str, out: &mut W) -> fmt::Result {
    let bytes = text.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if is_unreserved(b) {
            i += 1;
            continue;
        }
        if i > seg_start
            && let Some(chunk) = text.get(seg_start..i)
        {
            out.write_str(chunk)?;
        }
        if b == b' ' {
            out.write_char('+')?;
        } else {
            write_pct_into(b, out)?;
        }
        i += 1;
        seg_start = i;
    }
    if i > seg_start
        && let Some(chunk) = text.get(seg_start..i)
    {
        out.write_str(chunk)?;
    }
    Ok(())
}

fn form_encode_component_string<A: Allocator>(
    text: &str,
    out: &mut String<A>,
) -> Result<(), AllocFailure> {
    let bytes = text.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if is_unreserved(b) {
            i += 1;
            continue;
        }
        if i > seg_start
            && let Some(chunk) = text.get(seg_start..i)
        {
            out.try_push_str(chunk)?;
        }
        if b == b' ' {
            out.try_push('+')?;
        } else {
            let triple = pct_triple(b);
            if let Ok(s) = core::str::from_utf8(&triple) {
                out.try_push_str(s)?;
            }
        }
        i += 1;
        seg_start = i;
    }
    if i > seg_start
        && let Some(chunk) = text.get(seg_start..i)
    {
        out.try_push_str(chunk)?;
    }
    Ok(())
}

fn form_encoded_len(text: &str) -> usize {
    let mut total = 0usize;
    for &b in text.as_bytes() {
        let step = if is_unreserved(b) || b == b' ' { 1 } else { 3 };
        total = total.saturating_add(step);
    }
    total
}

/// Format key/value pairs as an `application/x-www-form-urlencoded` query string into `out`.
///
/// Encodes `' '` as `+`, leaves RFC 3986 unreserved bytes literal, and encodes all other bytes
/// as uppercase `%XX`.
pub fn format_query_into<W: fmt::Write>(params: &[(&str, &str)], out: &mut W) -> fmt::Result {
    for (idx, &(key, value)) in params.iter().enumerate() {
        if idx > 0 {
            out.write_char('&')?;
        }
        form_encode_component_into(key, out)?;
        out.write_char('=')?;
        form_encode_component_into(value, out)?;
    }
    Ok(())
}

/// Format key/value pairs as an `application/x-www-form-urlencoded` query [`String`] in `alloc`.
pub fn format_query_in<A: Allocator + Copy>(
    params: &[(&str, &str)],
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut cap = 0usize;
    for (idx, &(key, value)) in params.iter().enumerate() {
        if idx > 0 {
            cap = cap.saturating_add(1);
        }
        cap = cap
            .saturating_add(form_encoded_len(key))
            .saturating_add(1)
            .saturating_add(form_encoded_len(value));
    }
    let mut out = String::try_with_capacity_in(cap, alloc)?;
    for (idx, &(key, value)) in params.iter().enumerate() {
        if idx > 0 {
            out.try_push('&')?;
        }
        form_encode_component_string(key, &mut out)?;
        out.try_push('=')?;
        form_encode_component_string(value, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sans-IO RFC 7578 multipart/form-data
// ---------------------------------------------------------------------------

fn validate_boundary(boundary: &str) -> Result<(), QueryError> {
    let bytes = boundary.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_BOUNDARY_LEN {
        return Err(QueryError::InvalidBoundary);
    }
    if bytes.first() == Some(&b' ') || bytes.last() == Some(&b' ') {
        return Err(QueryError::InvalidBoundary);
    }
    for &b in bytes {
        if !(0x20..=0x7E).contains(&b) {
            return Err(QueryError::InvalidBoundary);
        }
    }
    Ok(())
}

fn build_delim_buf(boundary: &str) -> ([u8; DELIM_BUF_LEN], usize) {
    let mut buf = [0u8; DELIM_BUF_LEN];
    let b = boundary.as_bytes();
    let total = CRLF_DASH_PREFIX_LEN + b.len();
    if let Some(prefix) = buf.get_mut(0..CRLF_DASH_PREFIX_LEN) {
        prefix.copy_from_slice(b"\r\n--");
    }
    if let Some(dest) = buf.get_mut(CRLF_DASH_PREFIX_LEN..total) {
        dest.copy_from_slice(b);
    }
    (buf, total)
}

#[inline]
fn is_safe_quoted_header_param(s: &str) -> bool {
    !s.as_bytes()
        .iter()
        .any(|&b| matches!(b, b'\r' | b'\n' | b'\0' | b'"' | b'\\'))
}

#[inline]
fn is_safe_header_value(s: &str) -> bool {
    !s.as_bytes().iter().any(|&b| matches!(b, b'\r' | b'\n' | b'\0'))
}

#[inline]
fn trim_owsp(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

struct Disposition<'a> {
    name: Option<&'a str>,
    filename: Option<&'a str>,
}

fn parse_content_disposition<'a>(
    val: &'a str,
    header_offset: usize,
) -> Result<Disposition<'a>, QueryError> {
    let bytes = val.as_bytes();
    let mut i = 0usize;
    while matches!(bytes.get(i), Some(&b' ' | &b'\t')) {
        i += 1;
    }

    let type_start = i;
    while let Some(&b) = bytes.get(i) {
        if b == b';' {
            break;
        }
        i += 1;
    }
    let disp_type = trim_owsp(val.get(type_start..i).unwrap_or(""));
    if !disp_type.eq_ignore_ascii_case("form-data") {
        return Err(QueryError::InvalidHeader { offset: header_offset });
    }

    let mut name: Option<&'a str> = None;
    let mut filename: Option<&'a str> = None;

    while bytes.get(i) == Some(&b';') {
        i += 1; // skip ';'
        while matches!(bytes.get(i), Some(&b' ' | &b'\t')) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let key_start = i;
        while let Some(&b) = bytes.get(i) {
            if b == b'=' || b == b';' {
                break;
            }
            i += 1;
        }
        let param_name = trim_owsp(val.get(key_start..i).unwrap_or(""));
        if param_name.is_empty() || bytes.get(i) != Some(&b'=') {
            return Err(QueryError::InvalidHeader { offset: header_offset });
        }
        i += 1; // skip '='
        while matches!(bytes.get(i), Some(&b' ' | &b'\t')) {
            i += 1;
        }

        let param_val = if bytes.get(i) == Some(&b'"') {
            i += 1; // skip opening '"'
            let val_start = i;
            let mut close_pos: Option<usize> = None;
            while let Some(&b) = bytes.get(i) {
                if b == b'"' {
                    close_pos = Some(i);
                    i += 1;
                    break;
                }
                i += 1;
            }
            let Some(val_end) = close_pos else {
                return Err(QueryError::InvalidHeader { offset: header_offset });
            };
            while matches!(bytes.get(i), Some(&b' ' | &b'\t')) {
                i += 1;
            }
            if i < bytes.len() && bytes.get(i) != Some(&b';') {
                return Err(QueryError::InvalidHeader { offset: header_offset });
            }
            val.get(val_start..val_end).unwrap_or("")
        } else {
            let val_start = i;
            while let Some(&b) = bytes.get(i) {
                if b == b';' {
                    break;
                }
                i += 1;
            }
            trim_owsp(val.get(val_start..i).unwrap_or(""))
        };

        if param_name.eq_ignore_ascii_case("name") {
            if name.is_some() {
                return Err(QueryError::InvalidHeader { offset: header_offset });
            }
            name = Some(param_val);
        } else if param_name.eq_ignore_ascii_case("filename") {
            if filename.is_some() {
                return Err(QueryError::InvalidHeader { offset: header_offset });
            }
            filename = Some(param_val);
        }
    }

    Ok(Disposition { name, filename })
}

/// Parse an RFC 7578 `multipart/form-data` payload into zero-copy [`Part`] slices in `alloc`.
pub fn parse_multipart_in<'a, A: Allocator + Copy>(
    body: &'a [u8],
    boundary: &str,
    limits: MultipartLimits,
    alloc: A,
) -> Result<Vec<Part<'a>, A>, QueryError> {
    if body.len() > limits.most_bytes {
        return Err(QueryError::InputTooLarge {
            bytes: body.len(),
            limit: limits.most_bytes,
        });
    }
    validate_boundary(boundary)?;

    let (delim_buf, total_delim_len) = build_delim_buf(boundary);
    let crlf_dash_boundary = delim_buf.get(0..total_delim_len).unwrap_or(&[]);
    let dash_boundary = delim_buf.get(2..total_delim_len).unwrap_or(&[]);

    // Locate the first `--<boundary>` either at offset 0 or after an optional preamble CRLF.
    let mut pos = if body.starts_with(dash_boundary) {
        0usize
    } else if let Some(found) = memmem::find(body, crlf_dash_boundary) {
        found + 2
    } else {
        return Err(QueryError::MissingBoundary);
    };

    let mut parts = Vec::new_in(alloc);

    loop {
        // Advance past `--<boundary>`.
        pos += dash_boundary.len();

        // Check for closing `--` delimiter.
        if body.get(pos..).is_some_and(|rest| rest.starts_with(b"--")) {
            return Ok(parts);
        }

        // Skip optional linear transport padding (`SP` / `HTAB`) per RFC 2046 §5.1.1.
        while matches!(body.get(pos), Some(&b' ' | &b'\t')) {
            pos += 1;
        }

        if !body.get(pos..).is_some_and(|rest| rest.starts_with(b"\r\n")) {
            return Err(QueryError::MalformedBoundary { offset: pos });
        }
        pos += 2;

        if parts.len() >= limits.most_parts {
            return Err(QueryError::TooManyParts { limit: limits.most_parts });
        }

        let part_start = pos;
        let mut header_count = 0usize;
        let mut saw_disposition = false;
        let mut part_name: Option<&'a str> = None;
        let mut part_filename: Option<&'a str> = None;
        let mut part_content_type: Option<&'a str> = None;

        // Parse CRLF-terminated header lines up to the blank `\r\n` separator.
        loop {
            if body.get(pos..).is_some_and(|rest| rest.starts_with(b"\r\n")) {
                pos += 2;
                break;
            }
            let Some(rest) = body.get(pos..) else {
                return Err(QueryError::UnterminatedHeaders { offset: pos });
            };
            let Some(rel_eol) = memmem::find(rest, b"\r\n") else {
                return Err(QueryError::UnterminatedHeaders { offset: pos });
            };
            header_count += 1;
            if header_count > limits.most_headers_per_part {
                return Err(QueryError::TooManyHeaders {
                    limit: limits.most_headers_per_part,
                });
            }

            let line_end = pos + rel_eol;
            let line_bytes = body.get(pos..line_end).unwrap_or(&[]);
            let Ok(line_str) = core::str::from_utf8(line_bytes) else {
                return Err(QueryError::InvalidHeader { offset: pos });
            };
            let Some((hdr_name, hdr_val)) = line_str.split_once(':') else {
                return Err(QueryError::InvalidHeader { offset: pos });
            };
            let hdr_name_trimmed = trim_owsp(hdr_name);
            let hdr_val_trimmed = trim_owsp(hdr_val);

            if hdr_name_trimmed.eq_ignore_ascii_case("content-disposition") {
                if saw_disposition {
                    return Err(QueryError::InvalidHeader { offset: pos });
                }
                saw_disposition = true;
                let disp = parse_content_disposition(hdr_val_trimmed, pos)?;
                part_name = disp.name;
                part_filename = disp.filename;
            } else if hdr_name_trimmed.eq_ignore_ascii_case("content-type") {
                part_content_type = Some(hdr_val_trimmed);
            }

            pos = line_end + 2;
        }

        let Some(name) = part_name else {
            return Err(QueryError::MissingPartName { offset: part_start });
        };

        let Some(after_headers) = body.get(pos..) else {
            return Err(QueryError::MissingClosingBoundary);
        };
        let Some(rel_delim) = memmem::find(after_headers, crlf_dash_boundary) else {
            return Err(QueryError::MissingClosingBoundary);
        };
        let body_end = pos + rel_delim;
        let part_body = body.get(pos..body_end).unwrap_or(&[]);

        parts
            .try_push(Part {
                name,
                filename: part_filename,
                content_type: part_content_type,
                body: part_body,
            })
            .map_err(|r| QueryError::Alloc(r.why))?;

        // Point `pos` at `--<boundary>` of the next delimiter.
        pos = body_end + 2;
    }
}

/// Format `parts` into an RFC 7578 `multipart/form-data` byte buffer `out`.
///
/// Validates that `boundary` is non-empty safe ASCII without CR/LF, that part header fields do not
/// contain header-injection characters, and that `\r\n--<boundary>` does not appear inside any
/// part body.
pub fn format_multipart_into<A: Allocator>(
    parts: &[Part<'_>],
    boundary: &str,
    out: &mut Vec<u8, A>,
) -> Result<(), QueryError> {
    format_multipart_into_planted(parts, boundary, out, false)
}

fn format_multipart_into_planted<A: Allocator>(
    parts: &[Part<'_>],
    boundary: &str,
    out: &mut Vec<u8, A>,
    plant_skip_collision_check: bool,
) -> Result<(), QueryError> {
    validate_boundary(boundary)?;

    let (delim_buf, total_delim_len) = build_delim_buf(boundary);
    let crlf_dash_boundary = delim_buf.get(0..total_delim_len).unwrap_or(&[]);
    let dash_boundary = delim_buf.get(2..total_delim_len).unwrap_or(&[]);

    // Validate every part before writing so a refusal leaves `out` untouched.
    for (idx, part) in parts.iter().enumerate() {
        if !is_safe_quoted_header_param(part.name) {
            return Err(QueryError::InvalidHeaderField { part_index: idx });
        }
        if let Some(filename) = part.filename
            && !is_safe_quoted_header_param(filename)
        {
            return Err(QueryError::InvalidHeaderField { part_index: idx });
        }
        if let Some(ct) = part.content_type
            && !is_safe_header_value(ct)
        {
            return Err(QueryError::InvalidHeaderField { part_index: idx });
        }
        if !plant_skip_collision_check && memmem::find(part.body, crlf_dash_boundary).is_some() {
            return Err(QueryError::BoundaryCollision { part_index: idx });
        }
    }

    for part in parts {
        out.try_extend_from_slice(dash_boundary).map_err(QueryError::Alloc)?;
        out.try_extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"")
            .map_err(QueryError::Alloc)?;
        out.try_extend_from_slice(part.name.as_bytes()).map_err(QueryError::Alloc)?;
        out.try_extend_from_slice(b"\"").map_err(QueryError::Alloc)?;
        if let Some(filename) = part.filename {
            out.try_extend_from_slice(b"; filename=\"").map_err(QueryError::Alloc)?;
            out.try_extend_from_slice(filename.as_bytes()).map_err(QueryError::Alloc)?;
            out.try_extend_from_slice(b"\"").map_err(QueryError::Alloc)?;
        }
        out.try_extend_from_slice(b"\r\n").map_err(QueryError::Alloc)?;
        if let Some(ct) = part.content_type {
            out.try_extend_from_slice(b"Content-Type: ").map_err(QueryError::Alloc)?;
            out.try_extend_from_slice(ct.as_bytes()).map_err(QueryError::Alloc)?;
            out.try_extend_from_slice(b"\r\n").map_err(QueryError::Alloc)?;
        }
        out.try_extend_from_slice(b"\r\n").map_err(QueryError::Alloc)?;
        out.try_extend_from_slice(part.body).map_err(QueryError::Alloc)?;
        out.try_extend_from_slice(b"\r\n").map_err(QueryError::Alloc)?;
    }

    out.try_extend_from_slice(dash_boundary).map_err(QueryError::Alloc)?;
    out.try_extend_from_slice(b"--\r\n").map_err(QueryError::Alloc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::{
        MultipartLimits, Part, QueryError, QueryLimits, format_multipart_into,
        format_multipart_into_planted, format_query_in, format_query_into, parse_multipart_in,
        parse_query_in, parse_query_in_planted, percent_decode_into, percent_decode_utf8_in,
        percent_encode_in, percent_encode_into,
    };

    #[test]
    fn percent_encode_decode_utf8_and_invalid() {
        let h = heap();
        let alloc = h.alloc();

        // Unreserved characters stay literal.
        let unreserved = b"AZaz09-._~";
        let enc = percent_encode_in(unreserved, alloc).expect("encode unreserved");
        assert_eq!(enc.as_str(), "AZaz09-._~");

        // Spaces, reserved punctuation, and UTF-8 encode as uppercase %XX.
        let utf8_enc = percent_encode_in("hello world/café+1=2%".as_bytes(), alloc)
            .expect("encode utf8");
        assert_eq!(utf8_enc.as_str(), "hello%20world%2Fcaf%C3%A9%2B1%3D2%25");

        let mut via_writer = OwnString::new_in(alloc);
        percent_encode_into("hello world/café+1=2%".as_bytes(), &mut via_writer)
            .expect("write percent");
        assert_eq!(via_writer.as_str(), utf8_enc.as_str());

        // Decode handles both uppercase and lowercase hex and keeps '+' literal.
        let dec = percent_decode_utf8_in("hello%20world%2fcaf%c3%a9+raw", alloc)
            .expect("decode utf8");
        assert_eq!(dec.as_str(), "hello world/café+raw");

        // Strict rejections on truncated and non-hex escapes.
        let mut buf = OwnVec::new_in(alloc);
        assert_eq!(
            percent_decode_into("abc%", &mut buf),
            Err(QueryError::TruncatedPercent { offset: 3 })
        );
        assert_eq!(
            percent_decode_into("abc%2", &mut buf),
            Err(QueryError::TruncatedPercent { offset: 3 })
        );
        assert_eq!(
            percent_decode_into("abc%ZZ", &mut buf),
            Err(QueryError::InvalidPercentHex { offset: 3 })
        );
        assert_eq!(
            percent_decode_into("abc%0g", &mut buf),
            Err(QueryError::InvalidPercentHex { offset: 3 })
        );
        assert_eq!(
            percent_decode_utf8_in("%FF%FE", alloc),
            Err(QueryError::InvalidUtf8 { valid_up_to: 0 })
        );
    }

    #[test]
    fn query_string_parsing_formatting_and_edge_cases() {
        let h = heap();
        let alloc = h.alloc();

        assert!(
            parse_query_in("", QueryLimits::SHIPPED, alloc)
                .expect("empty")
                .is_empty()
        );
        assert!(
            parse_query_in("?", QueryLimits::SHIPPED, alloc)
                .expect("question only")
                .is_empty()
        );

        let parsed = parse_query_in(
            "?a=hello+world&&flag&empty=&token=x=y=z&k%2B1=caf%C3%A9&",
            QueryLimits::SHIPPED,
            alloc,
        )
        .expect("parse query");

        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0].key.as_str(), "a");
        assert_eq!(parsed[0].value.as_str(), "hello world");
        assert_eq!(parsed[1].key.as_str(), "flag");
        assert_eq!(parsed[1].value.as_str(), "");
        assert_eq!(parsed[2].key.as_str(), "empty");
        assert_eq!(parsed[2].value.as_str(), "");
        assert_eq!(parsed[3].key.as_str(), "token");
        assert_eq!(parsed[3].value.as_str(), "x=y=z");
        assert_eq!(parsed[4].key.as_str(), "k+1");
        assert_eq!(parsed[4].value.as_str(), "café");

        let pairs = [("q", "rust blaze"), ("tag", "c++ & more"), ("safe", "a-z_0.9~")];
        let formatted = format_query_in(&pairs, alloc).expect("format query");
        assert_eq!(formatted.as_str(), "q=rust+blaze&tag=c%2B%2B+%26+more&safe=a-z_0.9~");

        let mut via_writer = OwnString::new_in(alloc);
        format_query_into(&pairs, &mut via_writer).expect("format into");
        assert_eq!(via_writer.as_str(), formatted.as_str());

        assert_eq!(
            parse_query_in("a=1&b=2", QueryLimits { most_bytes: 4, most_params: 10 }, alloc),
            Err(QueryError::InputTooLarge { bytes: 7, limit: 4 })
        );
        assert_eq!(
            parse_query_in("a=1&b=2", QueryLimits { most_bytes: 64, most_params: 1 }, alloc),
            Err(QueryError::TooManyParams { limit: 1 })
        );
    }

    #[test]
    fn multipart_text_binary_and_errors() {
        let h = heap();
        let alloc = h.alloc();

        let boundary = "----BlazeBoundary7MA4YWxkTrZu0gW";
        let bin_payload: &[u8] = &[0x00, 0xFF, b'\r', b'\n', b'-', b'-', 0x42, 0x7F];
        let edge_body: &[u8] = b"----BlazeBoundary7MA4YWxkTrZu0gW"; // no leading \r\n, safe!
        let input_parts = [
            Part {
                name: "title",
                filename: None,
                content_type: None,
                body: b"Hello, multipart!",
            },
            Part {
                name: "upload",
                filename: Some("report; final.bin"),
                content_type: Some("application/octet-stream"),
                body: bin_payload,
            },
            Part {
                name: "empty",
                filename: None,
                content_type: Some("text/plain; charset=utf-8"),
                body: b"",
            },
            Part {
                name: "prefix_only",
                filename: None,
                content_type: None,
                body: edge_body,
            },
        ];

        let mut wire = OwnVec::new_in(alloc);
        format_multipart_into(&input_parts, boundary, &mut wire).expect("format multipart");

        let parsed =
            parse_multipart_in(wire.as_slice(), boundary, MultipartLimits::SHIPPED, alloc)
                .expect("parse multipart");
        assert_eq!(parsed.as_slice(), &input_parts);

        // Boundary collision inside a part body is refused.
        let colliding = [
            Part { name: "ok", filename: None, content_type: None, body: b"fine" },
            Part {
                name: "bad",
                filename: None,
                content_type: None,
                body: b"before\r\n------BlazeBoundary7MA4YWxkTrZu0gW\r\nafter",
            },
        ];
        let mut out = OwnVec::new_in(alloc);
        assert_eq!(
            format_multipart_into(&colliding, boundary, &mut out),
            Err(QueryError::BoundaryCollision { part_index: 1 })
        );
        assert!(out.is_empty());

        // Header injection refused.
        let injected = [Part {
            name: "bad\"\r\nX-Injected: 1",
            filename: None,
            content_type: None,
            body: b"x",
        }];
        assert_eq!(
            format_multipart_into(&injected, boundary, &mut out),
            Err(QueryError::InvalidHeaderField { part_index: 0 })
        );

        // Invalid boundaries refused.
        assert_eq!(
            parse_multipart_in(b"", "", MultipartLimits::SHIPPED, alloc),
            Err(QueryError::InvalidBoundary)
        );
        assert_eq!(
            format_multipart_into(&[], "bad\r\nbound", &mut out),
            Err(QueryError::InvalidBoundary)
        );
        assert_eq!(
            format_multipart_into(&[], "trailing ", &mut out),
            Err(QueryError::InvalidBoundary)
        );

        // Limits enforced.
        assert_eq!(
            parse_multipart_in(
                wire.as_slice(),
                boundary,
                MultipartLimits { most_bytes: 1024, most_parts: 2, most_headers_per_part: 8 },
                alloc,
            ),
            Err(QueryError::TooManyParts { limit: 2 })
        );
    }

    fn draw_query_pairs<A: Allocator + Copy>(
        tape: &Tape,
        alloc: A,
    ) -> OwnVec<(OwnString<A>, OwnString<A>), A> {
        const PIECES: &[&str] =
            &["", "abc", " ", "  ", "+", "&", "=", "%", "%20", "?", "-", ".", "_", "~", "café", "🦀"];
        let count = tape.int_in(0usize..=8, "param_count");
        let mut pairs = OwnVec::new_in(alloc);
        for _ in 0..count {
            let k_len = tape.int_in(0usize..=4, "key_pieces");
            let mut k = OwnString::new_in(alloc);
            for _ in 0..k_len {
                if let Some(&p) = tape.pick(PIECES, "key_piece") {
                    k.push_str(p);
                }
            }
            let v_len = tape.int_in(0usize..=4, "val_pieces");
            let mut v = OwnString::new_in(alloc);
            for _ in 0..v_len {
                if let Some(&p) = tape.pick(PIECES, "val_piece") {
                    v.push_str(p);
                }
            }
            pairs.push((k, v));
        }
        pairs
    }

    fn check_query_round_trip<A: Allocator + Copy>(
        tape: &Tape,
        pairs: &[(OwnString<A>, OwnString<A>)],
        plant_skip_plus_space: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut refs = OwnVec::with_capacity_in(pairs.len(), alloc);
        for (k, v) in pairs {
            refs.push((k.as_str(), v.as_str()));
        }
        let encoded = format_query_in(refs.as_slice(), alloc)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "format failed: {e}").as_str().into()))?;
        let mut query_input = OwnString::new_in(alloc);
        if tape.coin("leading_q") {
            query_input.push('?');
        }
        query_input.push_str(encoded.as_str());

        let parsed = parse_query_in_planted(
            query_input.as_str(),
            QueryLimits::SHIPPED,
            alloc,
            plant_skip_plus_space,
        )
        .map_err(|e| Broke::Oracle(format_in!(alloc, "parse failed: {e}").as_str().into()))?;

        if parsed.len() != pairs.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "len mismatch: expected {}, got {}", pairs.len(), parsed.len())
                    .as_str()
                    .into(),
            ));
        }
        for (idx, (got, (ek, ev))) in parsed.iter().zip(pairs.iter()).enumerate() {
            if got.key.as_str() != ek.as_str() || got.value.as_str() != ev.as_str() {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "param {idx} mismatch: expected ({:?}, {:?}), got ({:?}, {:?})",
                        ek.as_str(),
                        ev.as_str(),
                        got.key.as_str(),
                        got.value.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn property_percent_encode_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            let len = tape.int_in(0usize..=64, "byte_len");
            let mut raw = OwnVec::with_capacity_in(len, alloc);
            raw.resize(len, 0u8);
            tape.fill(raw.as_mut_slice(), "raw_bytes");

            let encoded = percent_encode_in(raw.as_slice(), alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "encode: {e}").as_str().into()))?;
            let mut decoded = OwnVec::new_in(alloc);
            percent_decode_into(encoded.as_str(), &mut decoded)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "decode: {e}").as_str().into()))?;

            if decoded.as_slice() != raw.as_slice() {
                return Err(Broke::Oracle(
                    format_in!(alloc, "percent round-trip mismatch").as_str().into(),
                ));
            }
            Ok(())
        });
    }

    /// The invariant: `format_query_in` and `parse_query_in` round-trip arbitrary key/value pairs including spaces encoded as `+`.
    #[test]
    fn property_query_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let pairs = draw_query_pairs(tape, h.alloc());
            check_query_round_trip(tape, pairs.as_slice(), false, h.alloc())
        });
    }

    #[test]
    fn property_multipart_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let alloc = h.alloc();
            const BOUNDARIES: &[&str] = &["BOUND123", "----BlazePart_99", "x", "a-b.c_d"];
            const NAMES: &[&str] = &["field", "file_1", "meta", "empty"];
            const FILENAMES: &[Option<&str>] =
                &[None, Some("a.txt"), Some("semi;colon.bin"), Some("photo.png")];
            const TYPES: &[Option<&str>] =
                &[None, Some("text/plain"), Some("application/octet-stream")];

            let boundary = tape.pick(BOUNDARIES, "boundary").copied().unwrap_or("BOUND123");
            let count = tape.int_in(0usize..=5, "part_count");

            let mut bodies: OwnVec<OwnVec<u8, _>, _> = OwnVec::new_in(alloc);
            let mut meta: OwnVec<(&str, Option<&str>, Option<&str>), _> = OwnVec::new_in(alloc);
            for _ in 0..count {
                let name = tape.pick(NAMES, "name").copied().unwrap_or("field");
                let filename = tape.pick(FILENAMES, "filename").copied().unwrap_or(None);
                let ct = tape.pick(TYPES, "ct").copied().unwrap_or(None);
                let blen = tape.int_in(0usize..=48, "body_len");
                let mut body = OwnVec::with_capacity_in(blen, alloc);
                body.resize(blen, 0u8);
                tape.fill(body.as_mut_slice(), "body_bytes");
                bodies.push(body);
                meta.push((name, filename, ct));
            }

            let mut parts: OwnVec<Part<'_>, _> = OwnVec::new_in(alloc);
            for (m, b) in meta.iter().zip(bodies.iter()) {
                parts.push(Part {
                    name: m.0,
                    filename: m.1,
                    content_type: m.2,
                    body: b.as_slice(),
                });
            }

            let mut wire = OwnVec::new_in(alloc);
            match format_multipart_into(parts.as_slice(), boundary, &mut wire) {
                Ok(()) => {
                    let parsed = parse_multipart_in(
                        wire.as_slice(),
                        boundary,
                        MultipartLimits::SHIPPED,
                        alloc,
                    )
                    .map_err(|e| {
                        Broke::Oracle(format_in!(alloc, "parse failed: {e}").as_str().into())
                    })?;
                    if parsed.as_slice() != parts.as_slice() {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "multipart round-trip mismatch").as_str().into(),
                        ));
                    }
                    Ok(())
                }
                Err(QueryError::BoundaryCollision { .. }) => Ok(()),
                Err(e) => Err(Broke::Oracle(
                    format_in!(alloc, "unexpected format error: {e}").as_str().into(),
                )),
            }
        });
    }

    #[test]
    fn the_oracle_catches_missing_plus_to_space() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let pairs = draw_query_pairs(tape, h.alloc());
            check_query_round_trip(tape, pairs.as_slice(), plant, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_boundary_collision_check() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let alloc = h.alloc();
            let boundary = "BLAZE_B";
            let mut body = OwnVec::new_in(alloc);
            body.extend_from_slice(b"prefix");
            if tape.coin("inject_boundary") {
                body.extend_from_slice(b"\r\n--BLAZE_B\r\n");
            }
            body.extend_from_slice(b"suffix");

            let parts = [Part {
                name: "payload",
                filename: None,
                content_type: None,
                body: body.as_slice(),
            }];
            let mut wire = OwnVec::new_in(alloc);
            match format_multipart_into_planted(&parts, boundary, &mut wire, plant) {
                Err(QueryError::BoundaryCollision { part_index: 0 }) => Ok(()),
                Err(e) => Err(Broke::Oracle(
                    format_in!(alloc, "unexpected format error: {e}").as_str().into(),
                )),
                Ok(()) => {
                    let parsed = parse_multipart_in(
                        wire.as_slice(),
                        boundary,
                        MultipartLimits::SHIPPED,
                        alloc,
                    )
                    .map_err(|e| {
                        Broke::Oracle(format_in!(alloc, "corrupt multipart: {e}").as_str().into())
                    })?;
                    if parsed.as_slice() != parts.as_slice() {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "boundary collision corrupted part body")
                                .as_str()
                                .into(),
                        ));
                    }
                    Ok(())
                }
            }
        });
    }
}
