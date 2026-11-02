use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::native::core::{EngineError, ManyState, Status};
use crate::native::intervalsets::IntervalSet;
use crate::sys::sync::Lazy;

use super::{Draws, many_more};
use crate::control::{InternalError, hegel_internal_debug_assert, hegel_internal_unwrap};

/// The IANA top-level-domain list, vendored from
/// `http://data.iana.org/TLD/tlds-alpha-by-domain.txt`. Same file Hypothesis
/// ships at `hypothesis/vendor/tlds-alpha-by-domain.txt`. One TLD per line,
/// uppercase, with a leading `#`-comment line carrying the version stamp.
const IANA_TLDS_TXT: &str = include_str!("tlds-alpha-by-domain.txt");

/// Eligible TLDs, with `ARPA` removed (RFC 3172 reserved infrastructure
/// domain — Hypothesis specifically drops it so generated addresses don't
/// look like `.in-addr.arpa` reverse-lookup names) and `COM` moved to the
/// front so the shrink target is `.com`. Mirrors
/// `hypothesis.provisional.get_top_level_domains`.
static TOP_LEVEL_DOMAINS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut sorted: Vec<&'static str> = IANA_TLDS_TXT
        .lines()
        .filter(|line| !line.starts_with('#') && *line != "ARPA")
        .collect();
    sorted.sort_by_key(|s| s.len());
    let mut result = Vec::with_capacity(sorted.len() + 1);
    result.push("COM");
    result.extend(sorted);
    result
});

/// RFC 5322 atext set used by Hypothesis's `emails()` for the local part:
/// `string.ascii_letters + string.digits + "!#$%&'*+-/=^_\`{|}~"`. Encoded
/// here as merged codepoint intervals in ascending order.
static EMAIL_LOCAL_PART_INTERVALS: Lazy<Result<Arc<IntervalSet>, InternalError>> =
    Lazy::new(|| {
        let ranges = vec![
            (b'!' as u32, b'!' as u32),
            (b'#' as u32, b'\'' as u32),
            (b'*' as u32, b'+' as u32),
            (b'-' as u32, b'-' as u32),
            (b'/' as u32, b'9' as u32),
            (b'=' as u32, b'=' as u32),
            (b'A' as u32, b'Z' as u32),
            (b'^' as u32, b'`' as u32),
            (b'a' as u32, b'z' as u32),
            (b'{' as u32, b'~' as u32),
        ];
        Ok(Arc::new(IntervalSet::new(ranges)?))
    });

/// `string.printable` from Python: ASCII 32..=126 plus the whitespace
/// codepoints `\t \n \x0b \x0c \r` (9..=13). Used for URL path components
/// before url-encoding.
static PRINTABLE_ASCII_INTERVALS: Lazy<Result<Arc<IntervalSet>, InternalError>> =
    Lazy::new(|| Ok(Arc::new(IntervalSet::new(vec![(9, 13), (32, 126)])?)));

/// Latin-1 byte range used as the alphabet for URL fragment characters.
/// Mirrors Hypothesis's `st.characters(min_codepoint=0, max_codepoint=255)`
/// for `_url_fragments_strategy`. Surrogates `[0xD800, 0xDFFF]` aren't in
/// range so the single interval is already surrogate-free.
static FRAGMENT_BYTE_INTERVALS: Lazy<Result<Arc<IntervalSet>, InternalError>> =
    Lazy::new(|| Ok(Arc::new(IntervalSet::new(vec![(0, 0xFF)])?)));

fn mark_invalid<D: Draws>(d: &mut D) -> Result<String, EngineError> {
    d.conclude(Status::Invalid);
    Err(EngineError::InvalidTestCase)
}

/// A validated domain-name draw: the RFC 1035 length cap plus the eligible
/// TLD list it admits, both fixed at construction time so draws never
/// re-filter the IANA list.
#[derive(Debug)]
pub struct DomainSpec {
    max_length: usize,
    eligible_tlds: Vec<&'static str>,
}

impl DomainSpec {
    /// Validate `max_length` against RFC 1035 §2.3.4 (a fully-qualified name
    /// is at most 255 octets, and must fit at least one label plus a TLD)
    /// and precompute the TLDs that fit within it.
    pub(crate) fn new(max_length: usize) -> Result<Self, EngineError> {
        if max_length > 255 {
            return Err(EngineError::InvalidArgument(format!(
                "domain max_length={max_length} exceeds the RFC 1035 limit of 255"
            )));
        }
        let eligible_tlds: Vec<&'static str> = TOP_LEVEL_DOMAINS
            .iter()
            .copied()
            .filter(|tld| tld.len() + 2 <= max_length)
            .collect();
        if eligible_tlds.is_empty() {
            return Err(EngineError::InvalidArgument(format!(
                "domain max_length={max_length} leaves no eligible TLDs"
            )));
        }
        Ok(DomainSpec {
            max_length,
            eligible_tlds,
        })
    }
}

/// The full-length domain spec used by email and URL draws. `max_length`
/// 255 admits every TLD, so a construction failure is an internal error,
/// deferred to the first draw that reads the spec.
static FULL_LENGTH_DOMAIN: Lazy<Result<DomainSpec, InternalError>> = Lazy::new(|| {
    let spec = DomainSpec::new(255).ok();
    Ok(hegel_internal_unwrap!(
        spec,
        "the full-length domain spec failed to build"
    ))
});

/// Draw an RFC 1035 fully-qualified domain name.
///
/// Port of Hypothesis's `DomainNameStrategy`:
///   1. Pick a TLD from the IANA list, filtered to `len(tld) + 2 <= max_length`.
///   2. Randomly flip the case of each TLD character.
///   3. Prepend 1..=126 dot-separated labels via `cu.many`, stopping early
///      if adding the next label would exceed `max_length`.
///
/// Each label matches the RFC 1035 regex
/// `[a-zA-Z]([a-zA-Z0-9\-]{0,61}[a-zA-Z0-9])?` and excludes labels that
/// start with `xn--` (RFC 5890 reserves these for punycode internationalised
/// domain names — Hypothesis filters them too). `max_length` follows
/// `DomainNameStrategy.max_length` (RFC 1035 §2.3.4): 4..=255.
pub(crate) fn generate_domain<D: Draws>(
    d: &mut D,
    spec: &DomainSpec,
) -> Result<String, EngineError> {
    const MAX_LABEL_LEN: usize = 63;

    let max_length = spec.max_length;
    let eligible = &spec.eligible_tlds;
    let idx = d.draw_integer_i64(0, eligible.len() as i64 - 1)? as usize;
    let tld = eligible[idx];

    let mut domain = String::with_capacity(tld.len());
    for c in tld.chars() {
        let flip = d.weighted(0.5, None)?;
        domain.push(if flip { c.to_ascii_lowercase() } else { c });
    }

    let mut state = ManyState::new(1, Some(126));
    loop {
        if !many_more(d, &mut state)? {
            break;
        }
        let label = draw_dns_label(d, MAX_LABEL_LEN)?;
        if domain.len() + 1 + label.len() > max_length {
            continue;
        }
        let mut next = String::with_capacity(label.len() + 1 + domain.len());
        next.push_str(&label);
        next.push('.');
        next.push_str(&domain);
        domain = next;
    }

    Ok(domain)
}

/// Draw one RFC 1035 / RFC 1123 label: starts with a letter, ends with
/// alphanumeric, middle is alphanumeric or hyphen, with the RFC 5890
/// reservation `label[2:4] != "--"` honoured inline.
///
/// Hypothesis enforces the RFC 5890 reservation as a filter-retry on the
/// regex strategy. Index 3 is the only middle position that can complete a
/// `--` window at indices `[2:4]`: the last position is always
/// alphanumeric, and position 1 is too close to the start. We inline that
/// constraint by coercing index 3 to alphanumeric whenever index 2 was
/// drawn as a hyphen, producing the same alphabet without a retry loop.
fn draw_dns_label<D: Draws>(d: &mut D, max_len: usize) -> Result<String, EngineError> {
    let len = d.draw_integer_i64(1, max_len as i64)? as usize;
    let mut s = String::with_capacity(len);
    s.push(draw_ascii_letter(d)?);
    if len > 1 {
        for i in 1..(len - 1) {
            let avoid_dash = i == 3 && s.as_bytes()[2] == b'-';
            s.push(if avoid_dash {
                draw_ascii_alnum(d)?
            } else {
                draw_ascii_alnum_or_hyphen(d)?
            });
        }
        s.push(draw_ascii_alnum(d)?);
    }
    Ok(s)
}

fn draw_ascii_letter<D: Draws>(d: &mut D) -> Result<char, EngineError> {
    let i = d.draw_integer_i64(0, 51)? as u8;
    let b = if i < 26 { b'a' + i } else { b'A' + (i - 26) };
    Ok(b as char)
}

fn draw_ascii_alnum<D: Draws>(d: &mut D) -> Result<char, EngineError> {
    let i = d.draw_integer_i64(0, 61)? as u8;
    let b = if i < 26 {
        b'a' + i
    } else if i < 52 {
        b'A' + (i - 26)
    } else {
        b'0' + (i - 52)
    };
    Ok(b as char)
}

fn draw_ascii_alnum_or_hyphen<D: Draws>(d: &mut D) -> Result<char, EngineError> {
    let i = d.draw_integer_i64(0, 62)? as u8;
    let b = if i < 26 {
        b'a' + i
    } else if i < 52 {
        b'A' + (i - 26)
    } else if i < 62 {
        b'0' + (i - 52)
    } else {
        b'-'
    };
    Ok(b as char)
}

/// Draw an RFC 5321/5322 email address like `alice@example.com`.
///
/// Port of Hypothesis's `emails()`:
///   - Local part: 1..=64 chars from the RFC 5322 atext set.
///   - Domain: as `generate_domain(255)`.
///   - Filter: overall length ≤ 254 (RFC 5321 §4.5.3.1.3). Implemented by
///     marking the test case invalid when the filter fails — the engine
///     then retries with a different choice prefix.
pub(crate) fn generate_email<D: Draws>(d: &mut D) -> Result<String, EngineError> {
    let alphabet = Arc::clone(EMAIL_LOCAL_PART_INTERVALS.as_ref().map_err(Clone::clone)?);
    let local = d.draw_string(alphabet, 1, 64)?;
    let domain_spec = FULL_LENGTH_DOMAIN.as_ref().map_err(Clone::clone)?;
    let domain = generate_domain(d, domain_spec)?;
    let address = format!("{local}@{domain}");
    if address.len() > 254 {
        return mark_invalid(d);
    }
    Ok(address)
}

/// Draw an RFC 3986 `http`/`https` URL.
///
/// Port of Hypothesis's `urls()` template
/// `"{scheme}://{domain}{port?}/{path}{fragment?}"`:
///   - `scheme` ∈ {http, https} (sampled_from).
///   - `domain` from `generate_domain(255)`.
///   - `port` is either empty or `:N` for `N ∈ 1..=65535`.
///   - `path` is the `/`-join of 0..N path components, each a url-encoded
///     `text(string.printable)` value.
///   - `fragment` is empty or `#…` of url-encoded chars in `0..=255`.
pub(crate) fn generate_url<D: Draws>(d: &mut D) -> Result<String, EngineError> {
    let scheme = if d.weighted(0.5, None)? {
        "https"
    } else {
        "http"
    };

    let domain_spec = FULL_LENGTH_DOMAIN.as_ref().map_err(Clone::clone)?;
    let domain = generate_domain(d, domain_spec)?;

    let port = if d.weighted(0.5, None)? {
        format!(":{}", d.draw_integer_i64(1, 65535)?)
    } else {
        String::new()
    };

    let mut state = ManyState::new(0, None);
    let mut components: Vec<String> = Vec::new();
    loop {
        if !many_more(d, &mut state)? {
            break;
        }
        let alphabet = Arc::clone(PRINTABLE_ASCII_INTERVALS.as_ref().map_err(Clone::clone)?);
        let raw = d.draw_string(alphabet, 0, 100)?;
        components.push(url_encode_path(&raw)?);
    }
    let path = components.join("/");

    let fragment = if d.weighted(0.5, None)? {
        let alphabet = Arc::clone(FRAGMENT_BYTE_INTERVALS.as_ref().map_err(Clone::clone)?);
        let raw = d.draw_string(alphabet, 1, 100)?;
        format!("#{}", url_encode_fragment(d, &raw)?)
    } else {
        String::new()
    };

    Ok(format!("{scheme}://{domain}{port}/{path}{fragment}"))
}

/// `URL_SAFE_CHARACTERS` from Hypothesis: `ascii_letters + digits +
/// "$-_.+!*'(),~"`. Characters outside this set are percent-encoded as
/// `%XX` where `XX` is the codepoint in uppercase hex.
fn is_url_safe(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '$' | '-' | '_' | '.' | '+' | '!' | '*' | '\'' | '(' | ')' | ',' | '~'
        )
}

/// `FRAGMENT_SAFE_CHARACTERS = URL_SAFE_CHARACTERS | {"?", "/"}`.
fn is_fragment_safe(c: char) -> bool {
    is_url_safe(c) || c == '?' || c == '/'
}

/// Apply Hypothesis's `url_encode` to a path component: each char is either
/// passed through (if URL-safe) or rendered as `%XX`.
fn url_encode_path(s: &str) -> Result<String, InternalError> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_url_safe(c) {
            out.push(c);
        } else {
            push_percent_encoded(&mut out, c)?;
        }
    }
    Ok(out)
}

/// Apply Hypothesis's `_url_fragments_strategy` per-char rule: with
/// probability 0.5 force `%XX`, otherwise `%XX` only if the char isn't in
/// `FRAGMENT_SAFE_CHARACTERS`.
fn url_encode_fragment<D: Draws>(d: &mut D, s: &str) -> Result<String, EngineError> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let force_encode = d.weighted(0.5, None)?;
        if force_encode || !is_fragment_safe(c) {
            push_percent_encoded(&mut out, c)?;
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Emit `%XX` for a single Latin-1 char. Inputs are bounded to codepoints
/// 0..=255 by the calling alphabets (`printable_ascii_intervals` and
/// `fragment_byte_intervals`), so the cast to `u8` is always exact.
fn push_percent_encoded(out: &mut String, c: char) -> Result<(), InternalError> {
    let cp = c as u32;
    hegel_internal_debug_assert!(
        cp <= 0xFF,
        "push_percent_encoded called with codepoint > 0xFF: {cp:#x}"
    );
    out.push_str(&format!("%{:02X}", cp & 0xFF));
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/draws/internet_tests.rs"]
mod tests;
