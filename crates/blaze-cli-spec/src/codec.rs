//! Pure sans-IO tab-separated cache codec for persisting [`CliSpec`] keyed by `(mtime, len)`.

use core::fmt::Write as _;

use blaze_own::{AString, Alloc, String};

use crate::{CliSpec, FlagSpec, SpecSource, SubcommandSpec};

/// Encode `spec` with binary stamp `(bin_mtime, bin_len)` into `alloc`.
pub fn encode_cache_in<'a>(
    spec: &CliSpec<'_>,
    bin_mtime: u128,
    bin_len: u64,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let _ = writeln!(out, "v1\t{bin_mtime}\t{bin_len}");
    for f in spec.flags.iter() {
        out.push_str("F\t");
        if let Some(l) = f.long.as_ref() {
            push_sanitized(&mut out, l.as_str());
        }
        out.push('\t');
        if let Some(s) = f.short.as_ref() {
            push_sanitized(&mut out, s.as_str());
        }
        out.push('\t');
        if let Some(h) = f.value_hint.as_ref() {
            push_sanitized(&mut out, h.as_str());
        }
        out.push('\t');
        push_sanitized(&mut out, f.description.as_str());
        out.push('\n');
    }
    for s in spec.subcommands.iter() {
        out.push_str("S\t");
        push_sanitized(&mut out, s.name.as_str());
        out.push('\t');
        push_sanitized(&mut out, s.description.as_str());
        out.push('\n');
    }
    out
}

/// Decode a cached spec only if its header matches `(bin_mtime, bin_len)`.
pub fn decode_cache_in<'a>(
    cmd: &str,
    raw: &str,
    bin_mtime: u128,
    bin_len: u64,
    alloc: Alloc<'a>,
) -> Option<CliSpec<'a>> {
    let mut lines = raw.lines();
    let header = lines.next()?;
    let mut expected = String::new_in(alloc);
    let _ = write!(expected, "v1\t{bin_mtime}\t{bin_len}");
    if header != expected.as_str() {
        return None;
    }
    let mut spec = CliSpec::empty_in(cmd, SpecSource::Help, alloc);
    for line in lines {
        let mut parts = line.splitn(5, '\t');
        match parts.next() {
            Some("F") => {
                let long = parts.next().unwrap_or("");
                let short = parts.next().unwrap_or("");
                let hint = parts.next().unwrap_or("");
                let desc = parts.next().unwrap_or("");
                spec.flags.push(FlagSpec {
                    long: (!long.is_empty()).then(|| String::from_str_in(long, alloc)),
                    short: (!short.is_empty()).then(|| String::from_str_in(short, alloc)),
                    value_hint: (!hint.is_empty()).then(|| String::from_str_in(hint, alloc)),
                    description: String::from_str_in(desc, alloc),
                });
            }
            Some("S") => {
                let name = parts.next().unwrap_or("");
                let desc = parts.next().unwrap_or("");
                if !name.is_empty() {
                    spec.subcommands.push(SubcommandSpec {
                        name: String::from_str_in(name, alloc),
                        description: String::from_str_in(desc, alloc),
                    });
                }
            }
            _ => {}
        }
    }
    Some(spec)
}

fn push_sanitized(out: &mut AString<'_>, s: &str) {
    for ch in s.chars() {
        if matches!(ch, '\t' | '\n' | '\r') {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manual::manual_spec_in;

    #[test]
    fn cache_round_trips_no_std() {
        let alloc = blaze_own::testing::heap().alloc();
        let spec = manual_spec_in("claude", None, alloc).expect("claude spec");
        let encoded = encode_cache_in(&spec, 42, 999, alloc);
        let decoded = decode_cache_in("claude", encoded.as_str(), 42, 999, alloc).expect("decode");
        assert_eq!(decoded.flags.len(), spec.flags.len());
        assert_eq!(decoded.subcommands.len(), spec.subcommands.len());
        assert!(decode_cache_in("claude", encoded.as_str(), 43, 999, alloc).is_none());
    }
}
