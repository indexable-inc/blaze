//! Pure `#![no_std]` RFC 6265 HTTP `Cookie` and `Set-Cookie` parser, validator, formatter,
//! and domain/path matcher.
//!
//! - Zero-copy parsing for `Cookie` and `Set-Cookie` name/value/domain/path slices.
//! - Strict validation of cookie names (RFC 7230 `token`), cookie values (RFC 6265 `cookie-value`),
//!   domains, paths, `Max-Age`, `SameSite`, and RFC 7231 IMF-fixdate `Expires` timestamps into
//!   [`crate::time::DateTime`].
//! - Formatting validates every field to prevent CRLF and semicolon header injection.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

use crate::time::{Date, DateTime, Time, Weekday};

const IMF_FIXDATE_LEN: usize = 29;
const MAX_DOMAIN_LEN: usize = 253;
const MAX_DOMAIN_LABEL_LEN: usize = 63;

/// RFC 6265bis `SameSite` cookie attribute.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SameSite {
    /// Cookie is sent only in first-party contexts (`SameSite=Strict`).
    Strict,
    /// Cookie is sent on top-level cross-site navigations (`SameSite=Lax`).
    Lax,
    /// Cookie is sent in all contexts (`SameSite=None`).
    None,
}

impl SameSite {
    /// Canonical `Set-Cookie` attribute value.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

impl fmt::Display for SameSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bounds enforced when parsing untrusted `Cookie` or `Set-Cookie` headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CookieLimits {
    /// Maximum header length in bytes.
    pub most_header_bytes: usize,
    /// Maximum number of cookie pairs parsed from a `Cookie` request header.
    pub most_cookies: usize,
}

impl CookieLimits {
    /// Generous default bounds (16 KiB header, 256 cookies).
    pub const USUAL: Self = Self {
        most_header_bytes: 16 * 1024,
        most_cookies: 256,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// A single `name=value` pair parsed from an HTTP `Cookie` request header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cookie<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

/// A parsed or outgoing HTTP `Set-Cookie` response header (RFC 6265 / RFC 6265bis).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SetCookie<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub expires: Option<DateTime>,
    pub max_age: Option<i64>,
    pub domain: Option<&'a str>,
    pub path: Option<&'a str>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
    pub partitioned: bool,
}

impl<'a> SetCookie<'a> {
    /// Construct a minimal [`SetCookie`] with `name` and `value` and all attributes unset.
    #[inline]
    #[must_use]
    pub const fn new(name: &'a str, value: &'a str) -> Self {
        Self {
            name,
            value,
            expires: None,
            max_age: None,
            domain: None,
            path: None,
            secure: false,
            http_only: false,
            same_site: None,
            partitioned: false,
        }
    }
}

/// Typed failure returned by cookie parsing, validation, or formatting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CookieError {
    /// Input header length exceeded [`CookieLimits::most_header_bytes`].
    HeaderTooLarge,
    /// `Cookie` header contained more pairs than [`CookieLimits::most_cookies`].
    TooManyCookies,
    /// A `name=value` cookie pair was missing `=`.
    MissingEquals,
    /// Cookie name was empty or contained bytes outside RFC 7230 `token`.
    InvalidName,
    /// Cookie value contained disallowed octets (such as CTLs, spaces, `;`, `,`, `\`, or unbalanced `"`).
    InvalidValue,
    /// An unrecognized or malformed `Set-Cookie` attribute was encountered.
    InvalidAttribute,
    /// `Max-Age` attribute was missing a value or not a valid RFC 6265 integer.
    InvalidMaxAge,
    /// `Expires` attribute was not a valid RFC 7231 IMF-fixdate or outside 4-digit UTC years.
    InvalidExpires,
    /// `SameSite` attribute was missing or not one of `Strict`, `Lax`, or `None`.
    InvalidSameSite,
    /// `Domain` attribute was empty or not a valid DNS domain name.
    InvalidDomain,
    /// `Path` attribute did not start with `/` or contained CTLs, `;`, or non-ASCII bytes.
    InvalidPath,
    /// Formatting into a [`core::fmt::Write`] sink failed.
    Format,
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for CookieError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl From<fmt::Error> for CookieError {
    #[inline]
    fn from(_err: fmt::Error) -> Self {
        Self::Format
    }
}

impl fmt::Display for CookieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CookieError::HeaderTooLarge => {
                f.write_str("bstd::cookie: header exceeds configured byte limit")
            }
            CookieError::TooManyCookies => {
                f.write_str("bstd::cookie: cookie count exceeds configured limit")
            }
            CookieError::MissingEquals => {
                f.write_str("bstd::cookie: cookie pair is missing `=` separator")
            }
            CookieError::InvalidName => {
                f.write_str("bstd::cookie: invalid cookie name (must be non-empty RFC 7230 token)")
            }
            CookieError::InvalidValue => {
                f.write_str("bstd::cookie: invalid cookie value (disallowed octets or unbalanced quotes)")
            }
            CookieError::InvalidAttribute => {
                f.write_str("bstd::cookie: unrecognized or malformed Set-Cookie attribute")
            }
            CookieError::InvalidMaxAge => {
                f.write_str("bstd::cookie: invalid Max-Age attribute value")
            }
            CookieError::InvalidExpires => {
                f.write_str("bstd::cookie: invalid Expires attribute (expected RFC 7231 IMF-fixdate)")
            }
            CookieError::InvalidSameSite => {
                f.write_str("bstd::cookie: invalid SameSite attribute (expected Strict, Lax, or None)")
            }
            CookieError::InvalidDomain => {
                f.write_str("bstd::cookie: invalid Domain attribute value")
            }
            CookieError::InvalidPath => {
                f.write_str("bstd::cookie: invalid Path attribute value")
            }
            CookieError::Format => f.write_str("bstd::cookie: formatting into writer failed"),
            CookieError::Alloc(err) => write!(f, "bstd::cookie: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for CookieError {}

/// Parse an HTTP `Cookie` request header into zero-copy [`Cookie`] pairs in `alloc`.
pub fn parse_cookie_header_in<'a, A: Allocator + Copy>(
    header: &'a str,
    limits: CookieLimits,
    alloc: A,
) -> Result<Vec<Cookie<'a>, A>, CookieError> {
    if header.len() > limits.most_header_bytes {
        return Err(CookieError::HeaderTooLarge);
    }

    let trimmed = trim_ows(header);
    let mut cookies = Vec::new_in(alloc);
    if trimmed.is_empty() {
        return Ok(cookies);
    }

    for segment in trimmed.split(';') {
        let part = trim_ows(segment);
        if part.is_empty() {
            continue;
        }
        let (raw_name, raw_value) = split_pair(part)?;
        let name = trim_ows(raw_name);
        let value = trim_ows(raw_value);
        if !is_valid_cookie_name(name) {
            return Err(CookieError::InvalidName);
        }
        if !is_valid_cookie_value(value) {
            return Err(CookieError::InvalidValue);
        }
        if cookies.len() >= limits.most_cookies {
            return Err(CookieError::TooManyCookies);
        }
        if let Err(refused) = cookies.try_push(Cookie { name, value }) {
            return Err(CookieError::Alloc(refused.why));
        }
    }

    Ok(cookies)
}

/// Parse an HTTP `Set-Cookie` response header into a zero-copy [`SetCookie`].
///
/// Validates name and value octets strictly and parses attributes case-insensitively:
/// `Expires` (RFC 7231 IMF-fixdate), `Max-Age`, `Domain` (stripping a single leading `.`),
/// `Path`, `Secure`, `HttpOnly`, `SameSite`, and `Partitioned`.
pub fn parse_set_cookie<'a>(
    header: &'a str,
    limits: CookieLimits,
) -> Result<SetCookie<'a>, CookieError> {
    if header.len() > limits.most_header_bytes {
        return Err(CookieError::HeaderTooLarge);
    }

    let mut segments = header.split(';');
    let Some(first_segment) = segments.next() else {
        return Err(CookieError::MissingEquals);
    };
    let (raw_name, raw_value) = split_pair(first_segment)?;
    let name = trim_ows(raw_name);
    let value = trim_ows(raw_value);
    if !is_valid_cookie_name(name) {
        return Err(CookieError::InvalidName);
    }
    if !is_valid_cookie_value(value) {
        return Err(CookieError::InvalidValue);
    }

    let mut out = SetCookie::new(name, value);

    for segment in segments {
        let attr = trim_ows(segment);
        if attr.is_empty() {
            continue;
        }
        let (attr_name, attr_val) = match split_optional_pair(attr) {
            (k, Some(v)) => (trim_ows(k), Some(trim_ows(v))),
            (k, None) => (trim_ows(k), None),
        };

        if attr_name.eq_ignore_ascii_case("expires") {
            let Some(raw) = attr_val else {
                return Err(CookieError::InvalidExpires);
            };
            out.expires = Some(parse_imf_fixdate(raw)?);
        } else if attr_name.eq_ignore_ascii_case("max-age") {
            let Some(raw) = attr_val else {
                return Err(CookieError::InvalidMaxAge);
            };
            out.max_age = Some(parse_max_age(raw)?);
        } else if attr_name.eq_ignore_ascii_case("domain") {
            let Some(raw) = attr_val else {
                return Err(CookieError::InvalidDomain);
            };
            let stripped = match raw.strip_prefix('.') {
                Some(rest) => rest,
                None => raw,
            };
            if !is_valid_domain(stripped) {
                return Err(CookieError::InvalidDomain);
            }
            out.domain = Some(stripped);
        } else if attr_name.eq_ignore_ascii_case("path") {
            let Some(raw) = attr_val else {
                return Err(CookieError::InvalidPath);
            };
            if !is_valid_path(raw) {
                return Err(CookieError::InvalidPath);
            }
            out.path = Some(raw);
        } else if attr_name.eq_ignore_ascii_case("secure") {
            if attr_val.is_some() {
                return Err(CookieError::InvalidAttribute);
            }
            out.secure = true;
        } else if attr_name.eq_ignore_ascii_case("httponly") {
            if attr_val.is_some() {
                return Err(CookieError::InvalidAttribute);
            }
            out.http_only = true;
        } else if attr_name.eq_ignore_ascii_case("samesite") {
            let Some(raw) = attr_val else {
                return Err(CookieError::InvalidSameSite);
            };
            let ss = if raw.eq_ignore_ascii_case("strict") {
                SameSite::Strict
            } else if raw.eq_ignore_ascii_case("lax") {
                SameSite::Lax
            } else if raw.eq_ignore_ascii_case("none") {
                SameSite::None
            } else {
                return Err(CookieError::InvalidSameSite);
            };
            out.same_site = Some(ss);
        } else if attr_name.eq_ignore_ascii_case("partitioned") {
            if attr_val.is_some() {
                return Err(CookieError::InvalidAttribute);
            }
            out.partitioned = true;
        } else {
            return Err(CookieError::InvalidAttribute);
        }
    }

    Ok(out)
}

/// Validate and format `cookie` as an HTTP `Set-Cookie` header value into `out`.
///
/// Validates `name`, `value`, `domain`, `path`, and `expires` before writing so CRLF or
/// semicolon header injection is impossible. `Expires` is formatted in RFC 7231 IMF-fixdate
/// UTC (`Thu, 01 Jan 1970 00:00:00 GMT`).
pub fn format_set_cookie_into<W: fmt::Write>(
    cookie: &SetCookie<'_>,
    out: &mut W,
) -> Result<(), CookieError> {
    if !is_valid_cookie_name(cookie.name) {
        return Err(CookieError::InvalidName);
    }
    if !is_valid_cookie_value(cookie.value) {
        return Err(CookieError::InvalidValue);
    }
    if let Some(domain) = cookie.domain
        && !is_valid_domain(domain)
    {
        return Err(CookieError::InvalidDomain);
    }
    if let Some(path) = cookie.path
        && !is_valid_path(path)
    {
        return Err(CookieError::InvalidPath);
    }
    let expires_utc = match cookie.expires {
        None => None,
        Some(dt) => {
            let Some(utc) = dt.to_utc() else {
                return Err(CookieError::InvalidExpires);
            };
            let year = utc.date.year();
            if !(0..=9_999).contains(&year) {
                return Err(CookieError::InvalidExpires);
            }
            Some(utc)
        }
    };

    out.write_str(cookie.name)?;
    out.write_char('=')?;
    out.write_str(cookie.value)?;

    if let Some(utc) = expires_utc {
        out.write_str("; Expires=")?;
        format_imf_fixdate_into(utc, out)?;
    }
    if let Some(max_age) = cookie.max_age {
        write!(out, "; Max-Age={max_age}")?;
    }
    if let Some(domain) = cookie.domain {
        out.write_str("; Domain=")?;
        out.write_str(domain)?;
    }
    if let Some(path) = cookie.path {
        out.write_str("; Path=")?;
        out.write_str(path)?;
    }
    if cookie.secure {
        out.write_str("; Secure")?;
    }
    if cookie.http_only {
        out.write_str("; HttpOnly")?;
    }
    if let Some(same_site) = cookie.same_site {
        out.write_str("; SameSite=")?;
        out.write_str(same_site.as_str())?;
    }
    if cookie.partitioned {
        out.write_str("; Partitioned")?;
    }

    Ok(())
}

/// Validate and format `cookie` as an HTTP `Set-Cookie` header value into a [`String`] in `alloc`.
pub fn format_set_cookie_in<A: Allocator + Copy>(
    cookie: &SetCookie<'_>,
    alloc: A,
) -> Result<String<A>, CookieError> {
    let mut out = String::new_in(alloc);
    let mut writer = FallibleStringWriter { out: &mut out, alloc_err: None };
    if let Err(err) = format_set_cookie_into(cookie, &mut writer) {
        if let Some(alloc_err) = writer.alloc_err {
            return Err(CookieError::Alloc(alloc_err));
        }
        return Err(err);
    }
    Ok(out)
}

/// RFC 6265 §5.1.3 domain matching: ASCII case-insensitive exact match or `.suffix` match
/// against a non-IP host name.
#[inline]
#[must_use]
pub fn domain_matches(cookie_domain: &str, request_host: &str) -> bool {
    domain_matches_planted(cookie_domain, request_host, false)
}

fn domain_matches_planted(
    cookie_domain: &str,
    request_host: &str,
    plant_skip_dot_boundary: bool,
) -> bool {
    let cd = match cookie_domain.strip_prefix('.') {
        Some(rest) => rest,
        None => cookie_domain,
    };
    if cd.is_empty()
        || request_host.is_empty()
        || cd.starts_with('.')
        || request_host.starts_with('.')
    {
        return false;
    }
    if cd.eq_ignore_ascii_case(request_host) {
        return true;
    }
    if is_ip_address(request_host) {
        return false;
    }
    if request_host.len() <= cd.len() {
        return false;
    }
    let split = request_host.len() - cd.len();
    if !plant_skip_dot_boundary && request_host.as_bytes().get(split - 1) != Some(&b'.') {
        return false;
    }
    match request_host.get(split..) {
        Some(suffix) => suffix.eq_ignore_ascii_case(cd),
        None => false,
    }
}

/// RFC 6265 §5.1.4 path matching: exact match or `/` boundary prefix match.
#[inline]
#[must_use]
pub fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if cookie_path.is_empty()
        || request_path.is_empty()
        || !cookie_path.starts_with('/')
        || !request_path.starts_with('/')
    {
        return false;
    }
    if cookie_path == request_path {
        return true;
    }
    let Some(rest) = request_path.strip_prefix(cookie_path) else {
        return false;
    };
    cookie_path.ends_with('/') || rest.starts_with('/')
}

struct FallibleStringWriter<'a, A: Allocator> {
    out: &'a mut String<A>,
    alloc_err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for FallibleStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.alloc_err.is_some() {
            return Err(fmt::Error);
        }
        if let Err(err) = self.out.try_push_str(s) {
            self.alloc_err = Some(err);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

#[inline]
fn trim_ows(s: &str) -> &str {
    s.trim_matches(|ch| ch == ' ' || ch == '\t')
}

fn split_pair(segment: &str) -> Result<(&str, &str), CookieError> {
    let Some(eq_idx) = segment.as_bytes().iter().position(|&b| b == b'=') else {
        return Err(CookieError::MissingEquals);
    };
    let Some((name, rest)) = segment.split_at_checked(eq_idx) else {
        return Err(CookieError::MissingEquals);
    };
    let Some(value) = rest.strip_prefix('=') else {
        return Err(CookieError::MissingEquals);
    };
    Ok((name, value))
}

fn split_optional_pair(segment: &str) -> (&str, Option<&str>) {
    let Some(eq_idx) = segment.as_bytes().iter().position(|&b| b == b'=') else {
        return (segment, None);
    };
    let Some((name, rest)) = segment.split_at_checked(eq_idx) else {
        return (segment, None);
    };
    let value = rest.strip_prefix('=');
    (name, value)
}

fn is_valid_cookie_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for &b in name.as_bytes() {
        let ok = matches!(
            b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                | b'0'..=b'9'
                | b'A'..=b'Z'
                | b'^'
                | b'_'
                | b'`'
                | b'a'..=b'z'
                | b'|'
                | b'~'
        );
        if !ok {
            return false;
        }
    }
    true
}

#[inline]
fn is_cookie_octet(b: u8) -> bool {
    matches!(
        b,
        0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E
    )
}

fn is_valid_cookie_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    let inner = if bytes.len() >= 2 && bytes.first() == Some(&b'"') && bytes.last() == Some(&b'"') {
        match bytes.get(1..bytes.len() - 1) {
            Some(slice) => slice,
            None => return false,
        }
    } else {
        bytes
    };
    for &b in inner {
        if !is_cookie_octet(b) {
            return false;
        }
    }
    true
}

fn is_valid_domain(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > MAX_DOMAIN_LEN {
        return false;
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > MAX_DOMAIN_LABEL_LEN {
            return false;
        }
        if label.as_bytes().first() == Some(&b'-') || label.as_bytes().last() == Some(&b'-') {
            return false;
        }
        for &b in label.as_bytes() {
            if !(b.is_ascii_alphanumeric() || b == b'-') {
                return false;
            }
        }
    }
    true
}

fn is_valid_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    for &b in path.as_bytes() {
        if !(0x20..=0x7E).contains(&b) || b == b';' {
            return false;
        }
    }
    true
}

fn is_ip_address(host: &str) -> bool {
    if host.contains(':') {
        return true;
    }
    let mut parts = 0usize;
    for part in host.split('.') {
        parts += 1;
        if parts > 4 || part.is_empty() || part.len() > 3 {
            return false;
        }
        let mut val = 0u16;
        for &b in part.as_bytes() {
            if !b.is_ascii_digit() {
                return false;
            }
            val = val * 10 + u16::from(b - b'0');
        }
        if val > 255 {
            return false;
        }
    }
    parts == 4
}

fn parse_max_age(raw: &str) -> Result<i64, CookieError> {
    let bytes = raw.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err(CookieError::InvalidMaxAge);
    };
    let digits = if first == b'-' {
        match bytes.get(1..) {
            Some(rest) if !rest.is_empty() => rest,
            _ => return Err(CookieError::InvalidMaxAge),
        }
    } else {
        bytes
    };
    for &b in digits {
        if !b.is_ascii_digit() {
            return Err(CookieError::InvalidMaxAge);
        }
    }
    match raw.parse::<i64>() {
        Ok(secs) => Ok(secs),
        Err(_) => Err(CookieError::InvalidMaxAge),
    }
}

fn parse_two_digits(bytes: &[u8], offset: usize) -> Result<u8, CookieError> {
    let Some(&b0) = bytes.get(offset) else {
        return Err(CookieError::InvalidExpires);
    };
    let Some(&b1) = bytes.get(offset + 1) else {
        return Err(CookieError::InvalidExpires);
    };
    if !b0.is_ascii_digit() || !b1.is_ascii_digit() {
        return Err(CookieError::InvalidExpires);
    }
    Ok((b0 - b'0') * 10 + (b1 - b'0'))
}

fn parse_four_digits(bytes: &[u8], offset: usize) -> Result<i32, CookieError> {
    let hi = parse_two_digits(bytes, offset)?;
    let lo = parse_two_digits(bytes, offset + 2)?;
    Ok(i32::from(hi) * 100 + i32::from(lo))
}

fn parse_weekday_short(s: &str) -> Option<Weekday> {
    match s {
        "Mon" => Some(Weekday::Monday),
        "Tue" => Some(Weekday::Tuesday),
        "Wed" => Some(Weekday::Wednesday),
        "Thu" => Some(Weekday::Thursday),
        "Fri" => Some(Weekday::Friday),
        "Sat" => Some(Weekday::Saturday),
        "Sun" => Some(Weekday::Sunday),
        _ => None,
    }
}

fn weekday_short(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

fn parse_month_short(s: &str) -> Option<u8> {
    match s {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

fn month_short(month: u8) -> Option<&'static str> {
    match month {
        1 => Some("Jan"),
        2 => Some("Feb"),
        3 => Some("Mar"),
        4 => Some("Apr"),
        5 => Some("May"),
        6 => Some("Jun"),
        7 => Some("Jul"),
        8 => Some("Aug"),
        9 => Some("Sep"),
        10 => Some("Oct"),
        11 => Some("Nov"),
        12 => Some("Dec"),
        _ => None,
    }
}

/// Strictly parse an RFC 7231 §7.1.1.1 IMF-fixdate (`Thu, 01 Jan 1970 00:00:00 GMT`).
fn parse_imf_fixdate(raw: &str) -> Result<DateTime, CookieError> {
    let bytes = raw.as_bytes();
    if bytes.len() != IMF_FIXDATE_LEN {
        return Err(CookieError::InvalidExpires);
    }
    let Some(day_name) = raw.get(0..3) else {
        return Err(CookieError::InvalidExpires);
    };
    let Some(expected_weekday) = parse_weekday_short(day_name) else {
        return Err(CookieError::InvalidExpires);
    };
    if bytes.get(3) != Some(&b',')
        || bytes.get(4) != Some(&b' ')
        || bytes.get(7) != Some(&b' ')
        || bytes.get(11) != Some(&b' ')
        || bytes.get(16) != Some(&b' ')
        || bytes.get(19) != Some(&b':')
        || bytes.get(22) != Some(&b':')
        || bytes.get(25) != Some(&b' ')
        || raw.get(26..29) != Some("GMT")
    {
        return Err(CookieError::InvalidExpires);
    }

    let day = parse_two_digits(bytes, 5)?;
    let Some(month_name) = raw.get(8..11) else {
        return Err(CookieError::InvalidExpires);
    };
    let Some(month) = parse_month_short(month_name) else {
        return Err(CookieError::InvalidExpires);
    };
    let year = parse_four_digits(bytes, 12)?;
    let hour = parse_two_digits(bytes, 17)?;
    let minute = parse_two_digits(bytes, 20)?;
    let second = parse_two_digits(bytes, 23)?;

    let date = match Date::new(year, month, day) {
        Ok(d) => d,
        Err(_) => return Err(CookieError::InvalidExpires),
    };
    if date.weekday() != expected_weekday {
        return Err(CookieError::InvalidExpires);
    }
    let time = match Time::new(hour, minute, second, 0) {
        Ok(t) => t,
        Err(_) => return Err(CookieError::InvalidExpires),
    };
    Ok(DateTime::utc(date, time))
}

fn format_imf_fixdate_into<W: fmt::Write>(utc: DateTime, out: &mut W) -> Result<(), CookieError> {
    let day_name = weekday_short(utc.date.weekday());
    let Some(month_name) = month_short(utc.date.month()) else {
        return Err(CookieError::InvalidExpires);
    };
    write!(
        out,
        "{day_name}, {:02} {month_name} {:04} {:02}:{:02}:{:02} GMT",
        utc.date.day(),
        utc.date.year(),
        utc.time.hour(),
        utc.time.minute(),
        utc.time.second(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_own::{format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::*;
    use crate::time::UtcOffset;

    #[test]
    fn parse_cookie_header_basic_and_limits() {
        let h = heap();
        let alloc = h.alloc();

        let empty = match parse_cookie_header_in("   \t ", CookieLimits::USUAL, alloc) {
            Ok(v) => v,
            Err(err) => panic!("empty cookie header failed: {err}"),
        };
        assert!(empty.is_empty());

        let parsed = match parse_cookie_header_in(
            "session=abc123; theme=\"dark\"; empty=; lang=en-US;",
            CookieLimits::USUAL,
            alloc,
        ) {
            Ok(v) => v,
            Err(err) => panic!("parse_cookie_header_in failed: {err}"),
        };
        assert_eq!(
            parsed.as_slice(),
            &[
                Cookie { name: "session", value: "abc123" },
                Cookie { name: "theme", value: "\"dark\"" },
                Cookie { name: "empty", value: "" },
                Cookie { name: "lang", value: "en-US" },
            ]
        );

        assert_eq!(
            parse_cookie_header_in("no_equals", CookieLimits::USUAL, alloc),
            Err(CookieError::MissingEquals)
        );
        assert_eq!(
            parse_cookie_header_in("bad name=1", CookieLimits::USUAL, alloc),
            Err(CookieError::InvalidName)
        );
        assert_eq!(
            parse_cookie_header_in("a=bad value", CookieLimits::USUAL, alloc),
            Err(CookieError::InvalidValue)
        );
        assert_eq!(
            parse_cookie_header_in(
                "a=1; b=2",
                CookieLimits { most_header_bytes: 100, most_cookies: 1 },
                alloc,
            ),
            Err(CookieError::TooManyCookies)
        );
        assert_eq!(
            parse_cookie_header_in(
                "a=1; b=2",
                CookieLimits { most_header_bytes: 4, most_cookies: 10 },
                alloc,
            ),
            Err(CookieError::HeaderTooLarge)
        );
    }

    #[test]
    fn parse_and_format_set_cookie_with_imf_fixdate() {
        let h = heap();
        let alloc = h.alloc();

        let header = "sid=xyz987; expires=Thu, 01 Jan 1970 00:00:00 GMT; max-age=3600; \
                      domain=.example.com; path=/app; SECURE; httponly; samesite=lax; partitioned";
        let sc = match parse_set_cookie(header, CookieLimits::USUAL) {
            Ok(v) => v,
            Err(err) => panic!("parse_set_cookie failed: {err}"),
        };
        let epoch = match (Date::new(1970, 1, 1), Time::new(0, 0, 0, 0)) {
            (Ok(d), Ok(t)) => DateTime::utc(d, t),
            _ => panic!("epoch date/time"),
        };
        assert_eq!(sc.name, "sid");
        assert_eq!(sc.value, "xyz987");
        assert_eq!(sc.expires, Some(epoch));
        assert_eq!(sc.max_age, Some(3600));
        assert_eq!(sc.domain, Some("example.com"));
        assert_eq!(sc.path, Some("/app"));
        assert!(sc.secure);
        assert!(sc.http_only);
        assert_eq!(sc.same_site, Some(SameSite::Lax));
        assert!(sc.partitioned);

        let formatted = match format_set_cookie_in(&sc, alloc) {
            Ok(s) => s,
            Err(err) => panic!("format_set_cookie_in failed: {err}"),
        };
        assert_eq!(
            formatted.as_str(),
            "sid=xyz987; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=3600; Domain=example.com; \
             Path=/app; Secure; HttpOnly; SameSite=Lax; Partitioned"
        );

        // Formatting a non-UTC DateTime converts to UTC GMT.
        let local_dt = match (Date::new(2026, 6, 15), Time::new(14, 30, 0, 0), UtcOffset::from_hm(2, 0)) {
            (Ok(d), Ok(t), Ok(off)) => DateTime::new(d, t, off),
            _ => panic!("local date/time"),
        };
        let mut tz_cookie = SetCookie::new("tz", "1");
        tz_cookie.expires = Some(local_dt);
        let tz_formatted = match format_set_cookie_in(&tz_cookie, alloc) {
            Ok(s) => s,
            Err(err) => panic!("tz format failed: {err}"),
        };
        assert_eq!(tz_formatted.as_str(), "tz=1; Expires=Mon, 15 Jun 2026 12:30:00 GMT");

        // Wrong weekday in Expires must be refused.
        assert_eq!(
            parse_set_cookie("a=1; Expires=Mon, 01 Jan 1970 00:00:00 GMT", CookieLimits::USUAL),
            Err(CookieError::InvalidExpires)
        );
        // Leading '+' on Max-Age refused.
        assert_eq!(
            parse_set_cookie("a=1; Max-Age=+60", CookieLimits::USUAL),
            Err(CookieError::InvalidMaxAge)
        );
        // Unknown attribute refused.
        assert_eq!(
            parse_set_cookie("a=1; Priority=High", CookieLimits::USUAL),
            Err(CookieError::InvalidAttribute)
        );
        // Value on boolean flag refused.
        assert_eq!(
            parse_set_cookie("a=1; Secure=true", CookieLimits::USUAL),
            Err(CookieError::InvalidAttribute)
        );
        // Double leading dot in domain refused.
        assert_eq!(
            parse_set_cookie("a=1; Domain=..example.com", CookieLimits::USUAL),
            Err(CookieError::InvalidDomain)
        );
    }

    #[test]
    fn refuses_crlf_and_header_injection() {
        let h = heap();
        let alloc = h.alloc();

        let bad_name = SetCookie::new("bad\r\nSet-Cookie: evil", "1");
        assert_eq!(format_set_cookie_in(&bad_name, alloc), Err(CookieError::InvalidName));

        let bad_value = SetCookie::new("ok", "val\r\nX-Injected: 1");
        assert_eq!(format_set_cookie_in(&bad_value, alloc), Err(CookieError::InvalidValue));

        let bad_semi_value = SetCookie::new("ok", "val; Secure");
        assert_eq!(format_set_cookie_in(&bad_semi_value, alloc), Err(CookieError::InvalidValue));

        let mut bad_domain = SetCookie::new("ok", "1");
        bad_domain.domain = Some("example.com\r\nEvil: 1");
        assert_eq!(format_set_cookie_in(&bad_domain, alloc), Err(CookieError::InvalidDomain));

        let mut bad_path = SetCookie::new("ok", "1");
        bad_path.path = Some("/app\r\nEvil: 1");
        assert_eq!(format_set_cookie_in(&bad_path, alloc), Err(CookieError::InvalidPath));

        let mut bad_path_semi = SetCookie::new("ok", "1");
        bad_path_semi.path = Some("/app; HttpOnly");
        assert_eq!(format_set_cookie_in(&bad_path_semi, alloc), Err(CookieError::InvalidPath));

        assert_eq!(
            parse_set_cookie("a=1\r\nEvil: x", CookieLimits::USUAL),
            Err(CookieError::InvalidValue)
        );
        assert_eq!(
            parse_set_cookie("a=1; Path=/foo\r\n", CookieLimits::USUAL),
            Err(CookieError::InvalidPath)
        );
    }

    #[test]
    fn rfc6265_domain_and_path_matching_edge_cases() {
        // Domain matching (RFC 6265 §5.1.3).
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("EXAMPLE.COM", "example.com"));
        assert!(domain_matches("example.com", "sub.example.com"));
        assert!(domain_matches("example.com", "a.b.EXAMPLE.COM"));
        assert!(domain_matches(".example.com", "sub.example.com"));
        assert!(!domain_matches("example.com", "evil-example.com"));
        assert!(!domain_matches("example.com", "evilexample.com"));
        assert!(!domain_matches("sub.example.com", "example.com"));
        assert!(!domain_matches("example.com", ".example.com"));
        assert!(!domain_matches("", "example.com"));
        assert!(!domain_matches("example.com", ""));
        assert!(domain_matches("127.0.0.1", "127.0.0.1"));
        assert!(!domain_matches("0.0.1", "127.0.0.1"));
        assert!(domain_matches("::1", "::1"));
        assert!(!domain_matches("1", "::1"));

        // Path matching (RFC 6265 §5.1.4).
        assert!(path_matches("/", "/"));
        assert!(path_matches("/", "/a/b"));
        assert!(path_matches("/a", "/a"));
        assert!(path_matches("/a", "/a/"));
        assert!(path_matches("/a", "/a/b"));
        assert!(!path_matches("/a", "/ab"));
        assert!(!path_matches("/a/", "/a"));
        assert!(path_matches("/a/", "/a/"));
        assert!(path_matches("/a/", "/a/b"));
        assert!(!path_matches("/a/b", "/a"));
        assert!(!path_matches("", "/a"));
        assert!(!path_matches("a", "/a"));
    }

    #[test]
    fn property_set_cookie_round_trip() {
        const NAMES: &[&str] = &["sid", "csrftoken", "__Host-auth", "prefs.v2", "a_b-c"];
        const VALUES: &[&str] = &["", "abc123", "\"quoted\"", "v1:2/3=4", "token~_.-!"];
        const DOMAINS: &[Option<&str>] = &[None, Some("example.com"), Some("api.sub.example.org")];
        const PATHS: &[Option<&str>] = &[None, Some("/"), Some("/api"), Some("/api/v1/")];
        const SAME_SITES: &[Option<SameSite>] = &[
            None,
            Some(SameSite::Strict),
            Some(SameSite::Lax),
            Some(SameSite::None),
        ];

        let h = heap();
        let alloc = h.alloc();
        check(Config::cases(256), |tape| -> Result<(), Broke> {
            let name = NAMES.get(tape.index(NAMES.len(), "name")).copied().unwrap_or("sid");
            let value = VALUES.get(tape.index(VALUES.len(), "value")).copied().unwrap_or("");
            let domain = DOMAINS.get(tape.index(DOMAINS.len(), "domain")).copied().unwrap_or(None);
            let path = PATHS.get(tape.index(PATHS.len(), "path")).copied().unwrap_or(None);
            let same_site = SAME_SITES
                .get(tape.index(SAME_SITES.len(), "same_site"))
                .copied()
                .unwrap_or(None);
            let secure = tape.coin("secure");
            let http_only = tape.coin("http_only");
            let partitioned = tape.coin("partitioned");
            let max_age = if tape.coin("has_max_age") {
                Some(tape.int_in(-10i64..=86_400i64, "max_age"))
            } else {
                None
            };
            let expires = if tape.coin("has_expires") {
                let unix_secs = tape.int_in(0i64..=4_102_444_800i64, "expires_secs");
                DateTime::from_unix_secs(unix_secs)
            } else {
                None
            };

            let original = SetCookie {
                name,
                value,
                expires,
                max_age,
                domain,
                path,
                secure,
                http_only,
                same_site,
                partitioned,
            };
            let encoded = format_set_cookie_in(&original, alloc)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "format failed: {e}").as_str().into()))?;
            let decoded = parse_set_cookie(encoded.as_str(), CookieLimits::USUAL)
                .map_err(|e| Broke::Oracle(format_in!(alloc, "parse failed: {e}").as_str().into()))?;
            if decoded != original {
                return Err(Broke::Oracle(
                    format_in!(alloc, "round-trip mismatch for `{}`", encoded.as_str())
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }

    fn run_domain_match_oracle(tape: &Tape, plant_skip_dot: bool) -> Result<(), Broke> {
        const DOMAINS: &[&str] = &["example.com", "api.blaze.dev", "co.uk", "service.internal"];
        const PREFIXES: &[&str] = &["evil-", "not", "sub-", "x"];
        const SUBS: &[&str] = &["www", "a.b", "API", "edge-1"];

        let h = heap();
        let alloc = h.alloc();
        let dom = DOMAINS.get(tape.index(DOMAINS.len(), "domain")).copied().unwrap_or("example.com");
        let sub = SUBS.get(tape.index(SUBS.len(), "sub")).copied().unwrap_or("www");
        let bad_prefix = PREFIXES.get(tape.index(PREFIXES.len(), "bad_prefix")).copied().unwrap_or("evil-");

        // Valid subdomain must match.
        let good_host = format_in!(alloc, "{sub}.{dom}");
        if !domain_matches_planted(dom, good_host.as_str(), plant_skip_dot) {
            return Err(Broke::Oracle("valid subdomain rejected".into()));
        }

        // Bogus suffix without dot boundary (e.g. evil-example.com vs example.com) must NOT match.
        let evil_host = format_in!(alloc, "{bad_prefix}{dom}");
        if domain_matches_planted(dom, evil_host.as_str(), plant_skip_dot) {
            return Err(Broke::Oracle("bogus suffix domain matched".into()));
        }

        Ok(())
    }

    #[test]
    fn control_catches_bogus_suffix_domain_matching() {
        control(
            Config::cases(64),
            false,
            true,
            "bogus suffix domain matched",
            run_domain_match_oracle,
        );
    }
}
