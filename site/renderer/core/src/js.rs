//! The few JavaScript built-ins the TS renderer's output depends on, with their exact behaviour: `String.prototype.trim`
//! and `\s` (JS whitespace is wider than ASCII), `String(number)`, `encodeURIComponent`, `String.prototype.replace`
//! with a string pattern (its `$` patterns), `slice` on UTF-16 code units and `new Date(iso).toISOString()`.

use crate::world::Scalar;

/// What `\s` matches in a JS regex and what `trim` removes: WhiteSpace and LineTerminator.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// The same set as a regex class body, for regex-lite (whose `\s` is ASCII only).
pub const SPACE_CLASS: &str = "\t\n\u{0b}\u{0c}\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `String(n)` for the numbers the site has.
pub fn number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// `String(v)` of a field value; `None` (a missing field) and null are `''` because the TS writes `v ?? ''`.
pub fn string(v: Option<&Scalar>) -> String {
    match v {
        None | Some(Scalar::Null) => String::new(),
        Some(Scalar::Bool(b)) => b.to_string(),
        Some(Scalar::Int(i)) => i.to_string(),
        Some(Scalar::Float(f)) => number(*f),
        Some(Scalar::Str(s)) => s.clone(),
    }
}

/// `Number(v)`.
pub fn to_number(v: Option<&Scalar>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Scalar::Null) => 0.0,
        Some(Scalar::Bool(b)) => f64::from(u8::from(*b)),
        Some(Scalar::Int(i)) => *i as f64,
        Some(Scalar::Float(f)) => *f,
        Some(Scalar::Str(s)) => {
            let t = trim(s);
            if t.is_empty() { 0.0 } else { t.parse().unwrap_or(f64::NAN) }
        }
    }
}

/// `s.padStart(n, '0')`, counting UTF-16 units.
pub fn pad_zero(s: &str, n: usize) -> String {
    let len = s.encode_utf16().count();
    if len >= n { s.to_string() } else { "0".repeat(n - len) + s }
}

/// `s.slice(0, n)` in UTF-16 units. A cut through a surrogate pair drops the pair (JS would keep a lone surrogate,
/// which a Rust string cannot hold).
pub fn slice_utf16(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `subject.replace(pattern, replacement)` with a string pattern: the first occurrence only, and `$$`, `$&`, `` $` ``
/// and `$'` in the replacement are expanded.
pub fn replace_first(subject: &str, pattern: &str, replacement: &str) -> String {
    let Some(at) = subject.find(pattern) else { return subject.to_string() };
    let (before, after) = (&subject[..at], &subject[at + pattern.len()..]);
    let mut out = String::with_capacity(subject.len() + replacement.len());
    out.push_str(before);
    let mut chars = replacement.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().map(|(_, n)| *n) {
            Some('$') => out.push('$'),
            Some('&') => out.push_str(pattern),
            Some('`') => out.push_str(before),
            Some('\'') => out.push_str(after),
            _ => {
                out.push('$');
                continue;
            }
        }
        chars.next();
    }
    out.push_str(after);
    out
}

fn digits(s: &[u8], n: usize) -> Option<u32> {
    if s.len() < n || !s[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s[..n].iter().fold(0, |a, d| a * 10 + u32::from(d - b'0')))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Milliseconds since the epoch of an ISO 8601 date-time as `new Date(iso)` reads it: `YYYY-MM-DD`, then optionally
/// `T` (or a space) and `HH:MM[:SS[.fff]]`, then `Z` or `+HH:MM`. A date-time without an offset is read in UTC, the
/// time zone Workers run in. `None` where `Date` gives an invalid date.
pub fn parse_date(iso: &str) -> Option<i64> {
    let s = iso.as_bytes();
    let y = i64::from(digits(s, 4)?);
    if s.get(4) != Some(&b'-') || s.get(7) != Some(&b'-') {
        return None;
    }
    let (mo, d) = (digits(&s[5..], 2)?, digits(&s[8..], 2)?);
    // JSC and V8 take any day 1 to 31 and roll it over (`2026-02-29` is 1 March), as `new Date` does.
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    let mut i = 10;
    if i == s.len() {
        return Some(ms);
    }
    if s[i] != b'T' && s[i] != b't' && s[i] != b' ' {
        return None;
    }
    i += 1;
    let h = digits(&s[i..], 2)?;
    if s.get(i + 2) != Some(&b':') {
        return None;
    }
    let mi = digits(&s[i + 3..], 2)?;
    i += 5;
    let mut sec = 0;
    let mut frac = 0i64;
    if s.get(i) == Some(&b':') {
        sec = digits(&s[i + 1..], 2)?;
        i += 3;
        if s.get(i) == Some(&b'.') {
            let start = i + 1;
            let mut j = start;
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
            }
            if j == start {
                return None;
            }
            let f = &iso[start..j.min(start + 3)];
            frac = format!("{f:0<3}").parse().ok()?;
            i = j;
        }
    }
    if h > 24 || mi > 59 || sec > 59 || (h == 24 && (mi, sec, frac) != (0, 0, 0)) {
        return None;
    }
    ms += i64::from(h) * 3_600_000 + i64::from(mi) * 60_000 + i64::from(sec) * 1000 + frac;
    match s.get(i) {
        None => Some(ms),
        Some(b'Z' | b'z') if i + 1 == s.len() => Some(ms),
        Some(&sign @ (b'+' | b'-')) => {
            let oh = digits(&s[i + 1..], 2)?;
            let rest = &s[i + 3..];
            let om = match rest {
                [b':', ..] => digits(&rest[1..], 2).filter(|_| rest.len() == 3)?,
                _ => digits(rest, 2).filter(|_| rest.len() == 2)?,
            };
            if oh > 23 || om > 59 {
                return None;
            }
            let off = i64::from(oh) * 3_600_000 + i64::from(om) * 60_000;
            Some(if sign == b'+' { ms - off } else { ms + off })
        }
        _ => None,
    }
}

/// `toISOString().slice(0, 10)` and `.slice(11, 16)`: `YYYY-MM-DD`, `HH:MM`.
pub fn iso_day_minute(ms: i64) -> (String, String) {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    (format!("{y:04}-{m:02}-{d:02}"), format!("{:02}:{:02}", rem / 3_600_000, rem / 60_000 % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let ms = parse_date("2026-10-24T19:47:12.345Z").unwrap();
        assert_eq!(iso_day_minute(ms), ("2026-10-24".into(), "19:47".into()));
        let ms = parse_date("2026-10-24T23:30:00+02:00").unwrap();
        assert_eq!(iso_day_minute(ms), ("2026-10-24".into(), "21:30".into()));
        assert_eq!(parse_date("2026-02-30"), parse_date("2026-03-02"));
        assert_eq!(parse_date("2026-02-32"), None);
        assert_eq!(parse_date("yesterday"), None);
    }

    #[test]
    fn replace() {
        assert_eq!(
            replace_first("a<!--r-->b<!--r-->", "<!--r-->", "[$$ $& $`]"),
            "a[$ <!--r--> a]b<!--r-->"
        );
    }
}
