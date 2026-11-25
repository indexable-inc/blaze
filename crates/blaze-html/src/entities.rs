//! Pure `#![no_std]` HTML5 character reference decoder: named entities (`&amp;`, `&middot;`,
//! `&mdash;`, `&nbsp;`, ...), decimal numeric references (`&#123;`), and hex numeric references
//! (`&#x1F525;`).

use blaze_own::{AString, Alloc};

/// Decode all HTML character references in `src` into a new `AString<'a>` in `alloc`.
pub fn decode_entities<'a>(src: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(src.len(), alloc);
    decode_entities_into(src, &mut out);
    out
}

/// Append `src` with HTML character references decoded into `out`.
pub fn decode_entities_into(src: &str, out: &mut AString<'_>) {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        if bytes.get(i).copied() == Some(b'&')
            && let Some((decoded, advance)) = try_decode_at(src, i)
        {
            out.push_str(decoded.as_str());
            i = i.saturating_add(advance);
            continue;
        }
        if let Some(rest) = src.get(i..)
            && let Some(ch) = rest.chars().next()
        {
            out.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            break;
        }
    }
}

enum Decoded {
    Static(&'static str),
    Char([u8; 4], usize),
}

impl Decoded {
    fn from_char(ch: char) -> Self {
        let mut buf = [0u8; 4];
        let n = ch.encode_utf8(&mut buf).len();
        Self::Char(buf, n)
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Char(buf, n) => {
                buf.get(..*n).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("")
            }
        }
    }
}

fn try_decode_at(src: &str, start: usize) -> Option<(Decoded, usize)> {
    let after_amp = src.get(start.checked_add(1)?..)?;
    // Cap scan window to 32 bytes so malformed `&foo` never scans far.
    let mut semi = None;
    for (idx, b) in after_amp.bytes().take(32).enumerate() {
        if b == b';' {
            semi = Some(idx);
            break;
        }
        if b == b'&' || b == b'<' || b.is_ascii_whitespace() {
            break;
        }
    }
    let semi_idx = semi?;
    let body = after_amp.get(..semi_idx)?;
    if body.is_empty() {
        return None;
    }
    let advance = semi_idx.checked_add(2)?;

    if let Some(num) = body.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            if hex.is_empty() || hex.len() > 8 {
                return None;
            }
            u32::from_str_radix(hex, 16).ok()?
        } else {
            if num.is_empty() || num.len() > 10 {
                return None;
            }
            num.parse::<u32>().ok()?
        };
        let ch = match code {
            0 => '\u{FFFD}',
            // Common Windows-1252 remappings defined by WHATWG HTML5 spec
            0x80 => '€',
            0x85 => '…',
            0x91 => '‘',
            0x92 => '’',
            0x93 => '“',
            0x94 => '”',
            0x95 => '•',
            0x96 => '–',
            0x97 => '—',
            0x99 => '™',
            other => char::from_u32(other).unwrap_or('\u{FFFD}'),
        };
        return Some((Decoded::from_char(ch), advance));
    }

    let named = match body {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00A0}",
        "ensp" => " ",
        "emsp" => " ",
        "thinsp" => " ",
        "middot" | "centerdot" => "·",
        "bull" | "bullet" => "•",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "copy" | "COPY" => "©",
        "reg" | "REG" => "®",
        "trade" => "™",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "minus" => "−",
        "rarr" => "→",
        "larr" => "←",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "rArr" => "⇒",
        "lArr" => "⇐",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "sect" => "§",
        "para" => "¶",
        "deg" => "°",
        "check" | "checkmark" => "✓",
        "cross" => "✗",
        "star" | "starf" => "★",
        "hearts" => "♥",
        "spades" => "♠",
        "clubs" => "♣",
        "diams" => "♦",
        "ne" => "≠",
        "le" => "≤",
        "ge" => "≥",
        "infin" => "∞",
        "approx" => "≈",
        "micro" => "µ",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        _ => return None,
    };
    Some((Decoded::Static(named), advance))
}
