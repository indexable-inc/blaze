//! Zero-alloc ANSI escape sequence parsing, stripping, terminal display width, and styling.
//!
//! Inspired by `Bun.stripANSI` and `Bun.stringWidth`, written pure `#![no_std]` with zero unsafe,
//! zero panicking slice indexes, and zero integer `as` casts:
//!
//! - [`segments`]: zero-alloc iterator splitting text into [`Segment::Text`] and
//!   [`Segment::Escape`] (handles CSI `ESC [`, OSC `ESC ]` terminated by `BEL` or `ST`,
//!   DCS/SOS/PM/APC strings, and 2-byte `ESC` sequences).
//! - [`has_ansi`]: fast zero-alloc predicate.
//! - [`strip_into`] / [`strip_in`]: removes all ANSI escape sequences.
//! - [`visible_width`] / [`char_width`]: terminal cell width ignoring ANSI escapes (ASCII fast
//!   path, control & combining marks = 0, East Asian Wide / Fullwidth / emoji = 2).
//! - [`Style`] / [`Color`] / [`Styled`]: zero-alloc `const`-constructible terminal styling.

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_own::{AllocFailure, String};

pub use crate::table::{wrap_ansi_in, wrap_ansi_into};

/// One piece yielded by [`segments`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Segment<'a> {
    /// Visible plain text containing no ANSI escape sequences.
    Text(&'a str),
    /// A single ANSI escape sequence starting with `\x1b`.
    Escape(&'a str),
}

/// Zero-alloc iterator splitting a string into [`Segment::Text`] and [`Segment::Escape`] slices.
#[derive(Clone, Debug)]
pub struct Segments<'a> {
    rest: &'a str,
}

/// Iterate over visible text and ANSI escape segments of `input` without allocating.
#[inline]
#[must_use]
pub const fn segments(input: &str) -> Segments<'_> {
    Segments { rest: input }
}

impl<'a> Iterator for Segments<'a> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = *self.rest.as_bytes().first()?;
        if first == 0x1b {
            let len = scan_escape_len(self.rest.as_bytes());
            let (esc, tail) = self.rest.split_at_checked(len).unwrap_or((self.rest, ""));
            self.rest = tail;
            return Some(Segment::Escape(esc));
        }
        let next_esc = self.rest.as_bytes().iter().position(|&b| b == 0x1b).unwrap_or(self.rest.len());
        let (txt, tail) = self.rest.split_at_checked(next_esc).unwrap_or((self.rest, ""));
        self.rest = tail;
        Some(Segment::Text(txt))
    }
}

/// Length in bytes of the ANSI escape sequence starting at `bytes[0] == 0x1b`.
fn scan_escape_len(bytes: &[u8]) -> usize {
    let Some(&second) = bytes.get(1) else {
        return bytes.len();
    };
    match second {
        // CSI: ESC [ <param 0x30..=0x3f>* <intermediate 0x20..=0x2f>* <final 0x40..=0x7e>
        b'[' => {
            let mut i = 2;
            while let Some(&b) = bytes.get(i) {
                if (0x30..=0x3f).contains(&b) {
                    i += 1;
                } else {
                    break;
                }
            }
            while let Some(&b) = bytes.get(i) {
                if (0x20..=0x2f).contains(&b) {
                    i += 1;
                } else {
                    break;
                }
            }
            if let Some(&b) = bytes.get(i)
                && (0x40..=0x7e).contains(&b)
            {
                i += 1;
            }
            i
        }
        // OSC (ESC ]), DCS (ESC P), SOS (ESC X), PM (ESC ^), APC (ESC _):
        // terminated by BEL (0x07) or ST (ESC \).
        b']' | b'P' | b'X' | b'^' | b'_' => {
            let mut i = 2;
            while let Some(&b) = bytes.get(i) {
                if b == 0x07 {
                    return i + 1;
                }
                if b == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            bytes.len()
        }
        // nF escape sequence: ESC <0x20..=0x2f>+ <0x30..=0x7e>
        0x20..=0x2f => {
            let mut i = 2;
            while let Some(&b) = bytes.get(i) {
                if (0x20..=0x2f).contains(&b) {
                    i += 1;
                } else {
                    break;
                }
            }
            if let Some(&b) = bytes.get(i)
                && (0x30..=0x7e).contains(&b)
            {
                i += 1;
            }
            i
        }
        // Standard 2-byte ESC sequence.
        0x30..=0x7e => 2,
        // Bare ESC followed by non-ASCII: consume only ESC so UTF-8 boundary stays valid.
        _ => 1,
    }
}

/// Return `true` if `input` contains any ANSI escape byte (`\x1b`).
#[inline]
#[must_use]
pub fn has_ansi(input: &str) -> bool {
    input.as_bytes().contains(&0x1b)
}

/// Write `input` with all ANSI escape sequences removed into `out`.
pub fn strip_into<W: fmt::Write>(input: &str, out: &mut W) -> fmt::Result {
    strip_into_planted(input, out, false)
}

fn strip_into_planted<W: fmt::Write>(
    input: &str,
    out: &mut W,
    plant_keep_osc: bool,
) -> fmt::Result {
    if !has_ansi(input) {
        return out.write_str(input);
    }
    for seg in segments(input) {
        match seg {
            Segment::Text(txt) => out.write_str(txt)?,
            Segment::Escape(esc) if plant_keep_osc && esc.starts_with("\x1b]") => {
                out.write_str(esc)?;
            }
            Segment::Escape(_) => {}
        }
    }
    Ok(())
}

/// Return a copy of `input` with all ANSI escape sequences stripped, allocated in `alloc`.
pub fn strip_in<A: Allocator>(input: &str, alloc: A) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for seg in segments(input) {
        if let Segment::Text(txt) = seg {
            out.try_push_str(txt)?;
        }
    }
    Ok(out)
}

/// Display column width of `input` in a terminal, ignoring all ANSI escape sequences.
#[must_use]
pub fn visible_width(input: &str) -> usize {
    let mut width = 0usize;
    for seg in segments(input) {
        if let Segment::Text(txt) = seg {
            if txt.is_ascii() {
                for &b in txt.as_bytes() {
                    if (0x20..=0x7e).contains(&b) {
                        width += 1;
                    }
                }
            } else {
                for ch in txt.chars() {
                    width += char_width(ch);
                }
            }
        }
    }
    width
}

/// Terminal column width of a single Unicode scalar (`0`, `1`, `2`, or `3`), delegating to
/// [`blaze_unicode::char_width`] (official UCD tables — one home, rule 30). Control characters
/// return `0`.
#[inline]
#[must_use]
pub fn char_width(ch: char) -> usize {
    blaze_unicode::char_width(ch).unwrap_or(0)
}

/// Terminal ANSI color (4-bit standard, 8-bit 256-palette, or 24-bit RGB truecolor).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    /// 8-bit xterm-256 palette index.
    Indexed(u8),
    /// 24-bit RGB truecolor.
    Rgb(u8, u8, u8),
}

impl Color {
    fn write_sgr(self, f: &mut fmt::Formatter<'_>, bg: bool) -> fmt::Result {
        let base = if bg { 10u8 } else { 0u8 };
        match self {
            Color::Black => write!(f, "{}", 30 + base),
            Color::Red => write!(f, "{}", 31 + base),
            Color::Green => write!(f, "{}", 32 + base),
            Color::Yellow => write!(f, "{}", 33 + base),
            Color::Blue => write!(f, "{}", 34 + base),
            Color::Magenta => write!(f, "{}", 35 + base),
            Color::Cyan => write!(f, "{}", 36 + base),
            Color::White => write!(f, "{}", 37 + base),
            Color::BrightBlack => write!(f, "{}", 90 + base),
            Color::BrightRed => write!(f, "{}", 91 + base),
            Color::BrightGreen => write!(f, "{}", 92 + base),
            Color::BrightYellow => write!(f, "{}", 93 + base),
            Color::BrightBlue => write!(f, "{}", 94 + base),
            Color::BrightMagenta => write!(f, "{}", 95 + base),
            Color::BrightCyan => write!(f, "{}", 96 + base),
            Color::BrightWhite => write!(f, "{}", 97 + base),
            Color::Indexed(n) => write!(f, "{};5;{n}", 38 + base),
            Color::Rgb(r, g, b) => write!(f, "{};2;{r};{g};{b}", 38 + base),
        }
    }
}

/// Composable, `const`-constructible ANSI SGR text style.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

impl Style {
    pub const PLAIN: Self = Self {
        fg: None,
        bg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
        strikethrough: false,
    };

    #[must_use]
    pub const fn new() -> Self {
        Self::PLAIN
    }

    #[must_use]
    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    #[must_use]
    pub const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    #[must_use]
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    #[must_use]
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    #[must_use]
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    #[must_use]
    pub const fn inverse(mut self) -> Self {
        self.inverse = true;
        self
    }

    #[must_use]
    pub const fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
            && !self.strikethrough
    }

    /// Wrap `value` for zero-alloc [`fmt::Display`] rendering with ANSI SGR prefix and reset.
    #[inline]
    #[must_use]
    pub const fn paint<T>(self, value: T) -> Styled<T> {
        Styled { style: self, value }
    }
}

/// Zero-alloc wrapper rendering `value` with [`Style`] SGR codes and `\x1b[0m` reset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Styled<T> {
    pub style: Style,
    pub value: T,
}

impl<T: fmt::Display> fmt::Display for Styled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.style.is_plain() {
            return self.value.fmt(f);
        }
        f.write_str("\x1b[")?;
        let mut first = true;
        let mut sep = |f: &mut fmt::Formatter<'_>| -> fmt::Result {
            if first {
                first = false;
                Ok(())
            } else {
                f.write_char(';')
            }
        };
        if self.style.bold {
            sep(f)?;
            f.write_str("1")?;
        }
        if self.style.dim {
            sep(f)?;
            f.write_str("2")?;
        }
        if self.style.italic {
            sep(f)?;
            f.write_str("3")?;
        }
        if self.style.underline {
            sep(f)?;
            f.write_str("4")?;
        }
        if self.style.inverse {
            sep(f)?;
            f.write_str("7")?;
        }
        if self.style.strikethrough {
            sep(f)?;
            f.write_str("9")?;
        }
        if let Some(fg) = self.style.fg {
            sep(f)?;
            fg.write_sgr(f, false)?;
        }
        if let Some(bg) = self.style.bg {
            sep(f)?;
            bg.write_sgr(f, true)?;
        }
        f.write_char('m')?;
        self.value.fmt(f)?;
        f.write_str("\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;

    #[test]
    fn strip_csi_osc_and_width() {
        let h = heap();
        let colored = "\x1b[1;32mhello\x1b[0m \x1b]8;;https://ix.dev\x07world\x1b]8;;\x1b\\!";
        assert!(has_ansi(colored));
        let plain = strip_in(colored, h.alloc()).expect("strip");
        assert_eq!(plain.as_str(), "hello world!");
        assert!(!has_ansi(plain.as_str()));
        assert_eq!(visible_width(colored), 12);

        assert_eq!(visible_width("日本"), 4);
        assert_eq!(visible_width("e\u{0301}"), 1);
        assert_eq!(visible_width("\x1b[31m🚀\x1b[0m hi"), 5);
    }

    /// The invariant: stripping ANSI styling recovers the original text and preserves visible width.
    #[test]
    fn property_strip_inverts_styled_and_preserves_width() {
        let h = heap();
        const WORDS: &[&str] = &["hello", "blaze", "café", "日本", "e\u{0301}", "123", ""];
        const COLORS: &[Option<Color>] = &[
            None,
            Some(Color::Red),
            Some(Color::Cyan),
            Some(Color::Indexed(208)),
            Some(Color::Rgb(12, 200, 99)),
        ];
        blaze_sim::check(Config::cases(256), |tape| {
            let word = WORDS[usize::try_from(tape.below(WORDS.len() as u64, "word")).unwrap_or(0)];
            let fg = COLORS[usize::try_from(tape.below(COLORS.len() as u64, "fg")).unwrap_or(0)];
            let bg = COLORS[usize::try_from(tape.below(COLORS.len() as u64, "bg")).unwrap_or(0)];
            let mut st = Style::new();
            if let Some(c) = fg {
                st = st.fg(c);
            }
            if let Some(c) = bg {
                st = st.bg(c);
            }
            if tape.coin("bold") {
                st = st.bold();
            }
            if tape.coin("underline") {
                st = st.underline();
            }

            let rendered = format_in!(h.alloc(), "{}", st.paint(word));
            let stripped = strip_in(rendered.as_str(), h.alloc()).expect("strip");
            assert_eq!(stripped.as_str(), word);
            assert!(!has_ansi(stripped.as_str()));
            assert_eq!(visible_width(rendered.as_str()), visible_width(word));
        });
    }

    #[test]
    fn the_oracle_catches_unstripped_osc() {
        let h = heap();
        blaze_sim::control(Config::cases(64), false, true, "", |tape, plant| {
            let label = if tape.coin("link") { "docs" } else { "home" };
            let input = format_in!(h.alloc(), "\x1b]8;;https://ix.dev\x07{label}\x1b]8;;\x1b\\");
            let mut out = String::new_in(h.alloc());
            strip_into_planted(input.as_str(), &mut out, plant).expect("write");
            if out.as_str() != label {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "expected {label:?}, got {:?}", out.as_str())
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }
}
