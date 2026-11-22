//! Text as the terminal will draw it: source bytes and messages cut into [`Cell`]s of known
//! width, every byte that could act on the terminal turned into a visible symbol first
//! ([`escape`]), and the [`Pen`] that writes lines of cells with as few SGR sequences as the
//! styles need while counting the columns it fills.

use blaze_own::{AString, AVec};
use blaze_text::Writes;

use super::{Caps, Colour};
use crate::frame::lex::Class;

/// How a cell is underlined.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum Under {
    #[default]
    None,
    Single,
    Curly,
}

/// A cell's look: an index into the terminal's own 16 colours or a 24-bit colour (the
/// literals' tint, truecolor only), weight, slant and lines.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) struct Style {
    pub(crate) fg: Option<u8>,
    /// SGR `38;2`: wins over `fg`.
    pub(crate) rgb: Option<[u8; 3]>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) strike: bool,
    pub(crate) inverse: bool,
    pub(crate) under: Under,
    pub(crate) under_colour: Option<u8>,
}

impl Style {
    pub(crate) const PLAIN: Style = Style {
        fg: None,
        rgb: None,
        bold: false,
        dim: false,
        italic: false,
        strike: false,
        inverse: false,
        under: Under::None,
        under_colour: None,
    };
    pub(crate) const DIM: Style = Style { dim: true, ..Style::PLAIN };
    pub(crate) const BOLD: Style = Style { bold: true, ..Style::PLAIN };

    pub(crate) const fn fg(fg: u8) -> Style {
        Style { fg: Some(fg), ..Style::PLAIN }
    }
}

/// How a cell stands in a fix's preview.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Diff {
    Same,
    Removed,
    Inserted,
}

/// What a cell is marked by: nothing, a secondary span, or a finding of a level. Ordered by
/// strength: where two marks cover one cell, the stronger is drawn.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Ink {
    None,
    Secondary,
    Note,
    Warning,
    Error,
}

impl Ink {
    /// Its colour in the terminal's palette: red, yellow, cyan, and blue for the quieter one.
    pub(crate) const fn colour(self) -> Option<u8> {
        match self {
            Ink::None => None,
            Ink::Secondary => Some(4),
            Ink::Note => Some(6),
            Ink::Warning => Some(3),
            Ink::Error => Some(1),
        }
    }
}

/// What a cell draws.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Show<'t> {
    /// Text that is safe as it is: a grapheme cluster of the source or a message.
    Str(&'t str),
    /// Spaces: an expanded tab, or the one cell past a line's end a mark at its end takes.
    Spaces(u8),
    /// A visible stand-in for a byte or character that would act on the terminal.
    Esc([u8; 12], u8),
}

/// One cell of a line: what it draws, where it starts (in the line's columns), how wide it is,
/// and the source byte it stands for (`u32::MAX` for text that is no source byte).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cell<'t> {
    pub(crate) byte: u32,
    pub(crate) col: u32,
    pub(crate) width: u32,
    pub(crate) show: Show<'t>,
    pub(crate) class: Class,
    pub(crate) diff: Diff,
    pub(crate) ink: Ink,
    /// Which mark of the frame inked it last (an index), for placing that mark's label.
    pub(crate) mark: u32,
}

impl Cell<'_> {
    pub(crate) fn text(&self) -> &str {
        match &self.show {
            Show::Str(s) => s,
            Show::Spaces(n) => &"        "[..usize::from(*n).min(8)],
            Show::Esc(bytes, len) => core::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or("?"),
        }
    }

    pub(crate) fn is_space(&self) -> bool {
        match self.show {
            Show::Str(s) => s.bytes().all(|b| b == b' '),
            Show::Spaces(_) => true,
            Show::Esc(..) => false,
        }
    }
}

/// Columns a tab advances to the next multiple of.
pub(crate) const TAB: u32 = 4;

/// A bug planted on purpose, for the controls of the tests (rule 53); `Clean` everywhere else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    Clean,
    /// A wide character advances the column by one, not two: every mark after it is drawn one
    /// column to the left of its text.
    #[cfg_attr(not(test), expect(dead_code, reason = "planted only by the tests' controls"))]
    WideOffByOne,
    /// Names on the span's line drawn bold in the error's red: the rainbow the emphasis budget
    /// forbids.
    #[cfg_attr(not(test), expect(dead_code, reason = "planted only by the tests' controls"))]
    LoudNames,
}

/// Whether `c` is drawn as a stand-in: C0 and C1 controls, DEL, the bidirectional controls
/// (a file that reorders what it shows, "Trojan Source") and the line and paragraph
/// separators. A tab is expanded by the caller and never reaches here in source text.
pub(crate) fn needs_escape(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{061c}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// The stand-in for `c` and its width: a control picture (`␛`) where the terminal draws
/// Unicode, caret notation (`^[`) where not, and `\u{202e}` for the rest.
pub(crate) fn escape(c: char, unicode: bool) -> ([u8; 12], u8, u32) {
    let mut buf = [0u8; 12];
    let cp = u32::from(c);
    if cp < 0x20 || cp == 0x7f {
        if unicode {
            let picture = if cp == 0x7f { '\u{2421}' } else { char::from_u32(0x2400 + cp).unwrap_or('?') };
            let len = picture.encode_utf8(&mut buf).len();
            return (buf, len as u8, 1);
        }
        buf[0] = b'^';
        buf[1] = if cp == 0x7f { b'?' } else { (cp as u8) + 0x40 };
        return (buf, 2, 2);
    }
    // `\u{` hex `}`: at most 10 bytes.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[..3].copy_from_slice(b"\\u{");
    let mut len = 3;
    let digits = (32 - cp.leading_zeros()).div_ceil(4).max(1);
    for at in (0..digits).rev() {
        buf[len] = HEX[((cp >> (at * 4)) & 0xf) as usize];
        len += 1;
    }
    buf[len] = b'}';
    len += 1;
    (buf, len as u8, len as u32)
}

/// `\xNN` for a byte that is not UTF-8.
pub(crate) fn escape_byte(byte: u8) -> ([u8; 12], u8, u32) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 12];
    buf[..2].copy_from_slice(b"\\x");
    buf[2] = HEX[usize::from(byte >> 4)];
    buf[3] = HEX[usize::from(byte & 0xf)];
    (buf, 4, 4)
}

/// Cells for `bytes` (source text starting at file byte `base`, or a message when `base` is
/// `u32::MAX`), appended to `cells`, columns counted on from `col`: tabs expanded, grapheme
/// clusters one cell each at their display width, everything [`needs_escape`] names and every
/// byte that is not UTF-8 a stand-in. Returns the column after the last cell.
pub(crate) fn push_cells<'t>(
    cells: &mut AVec<'_, Cell<'t>>,
    bytes: &'t [u8],
    base: u32,
    mut col: u32,
    class_of: &dyn Fn(u32) -> Class,
    diff: Diff,
    unicode: bool,
    plant: Plant,
) -> u32 {
    let at_byte = |offset: usize| if base == u32::MAX { u32::MAX } else { base + offset as u32 };
    let push = |cells: &mut AVec<'_, Cell<'t>>, col: &mut u32, byte: u32, show: Show<'t>, width: u32, class: Class| {
        cells.push(Cell { byte, col: *col, width, show, class, diff, ink: Ink::None, mark: u32::MAX });
        *col += match plant {
            Plant::WideOffByOne if width == 2 => 1,
            _ => width,
        };
    };
    let mut done = 0;
    while done < bytes.len() {
        let (valid, bad) = match core::str::from_utf8(&bytes[done..]) {
            Ok(text) => (text, 0),
            Err(error) => {
                let valid = core::str::from_utf8(&bytes[done..done + error.valid_up_to()]).unwrap_or("");
                (valid, error.error_len().unwrap_or(bytes.len() - done - error.valid_up_to()))
            }
        };
        let mut offset = done;
        for cluster in blaze_unicode::graphemes(valid) {
            let byte = at_byte(offset);
            let class = class_of(byte);
            if cluster.chars().any(|c| c == '\t' || needs_escape(c)) {
                // Split: each character its own cell, so a control inside a cluster is seen.
                let mut inner = offset;
                for c in cluster.chars() {
                    let byte = at_byte(inner);
                    if c == '\t' {
                        let width = TAB - col % TAB;
                        push(cells, &mut col, byte, Show::Spaces(width as u8), width, class);
                    } else if needs_escape(c) {
                        let (buf, len, width) = escape(c, unicode);
                        push(cells, &mut col, byte, Show::Esc(buf, len), width, Class::Escape);
                    } else {
                        let text = &valid[inner - done..inner - done + c.len_utf8()];
                        let width = blaze_unicode::str_width(text) as u32;
                        push(cells, &mut col, byte, Show::Str(text), width, class);
                    }
                    inner += c.len_utf8();
                }
            } else {
                let width = blaze_unicode::str_width(cluster) as u32;
                push(cells, &mut col, byte, Show::Str(cluster), width, class);
            }
            offset += cluster.len();
        }
        done += valid.len();
        for _ in 0..bad {
            let (buf, len, width) = escape_byte(bytes[done]);
            push(cells, &mut col, at_byte(done), Show::Esc(buf, len), width, Class::Escape);
            done += 1;
        }
    }
    col
}

/// The width `text` takes once drawn safely: what [`push_cells`] would make of it.
pub(crate) fn safe_width(text: &str, unicode: bool) -> u32 {
    let mut width = 0;
    for cluster in blaze_unicode::graphemes(text) {
        if cluster.chars().any(needs_escape) || cluster.contains('\t') {
            for c in cluster.chars() {
                width += match c {
                    '\t' => 1,
                    c if needs_escape(c) => escape(c, unicode).2,
                    c => blaze_unicode::char_width(c).unwrap_or(1) as u32,
                };
            }
        } else {
            width += blaze_unicode::str_width(cluster) as u32;
        }
    }
    width
}

/// The writer of the frame's lines: styles become SGR sequences only where the terminal has
/// colour, and only where the style changes; `cols` counts what the current line fills.
pub(crate) struct Pen<'o, 'a> {
    pub(crate) out: &'o mut AString<'a>,
    pub(crate) caps: Caps,
    pub(crate) cols: u32,
    now: Style,
}

impl<'o, 'a> Pen<'o, 'a> {
    pub(crate) fn new(out: &'o mut AString<'a>, caps: Caps) -> Pen<'o, 'a> {
        Pen { out, caps, cols: 0, now: Style::PLAIN }
    }

    fn colour(&self) -> bool {
        self.caps.colour != Colour::None
    }

    pub(crate) fn style(&mut self, style: Style) {
        if !self.colour() || style == self.now {
            return;
        }
        self.now = style;
        self.out.push_str("\x1b[0");
        if style.bold {
            self.out.push_str(";1");
        }
        if style.dim {
            self.out.push_str(";2");
        }
        if style.italic {
            self.out.push_str(";3");
        }
        if style.inverse {
            self.out.push_str(";7");
        }
        if style.strike {
            self.out.push_str(";9");
        }
        if let Some([r, g, b]) = style.rgb {
            self.out.str(";38;2;").int(r).str(";").int(g).str(";").int(b);
        } else if let Some(fg) = style.fg {
            let code = if fg < 8 { 30 + u32::from(fg) } else { 90 + u32::from(fg - 8) };
            self.out.str(";").int(code);
        }
        match style.under {
            Under::None => {}
            Under::Single => self.out.push_str(";4"),
            Under::Curly if self.caps.curly => self.out.push_str(";4:3"),
            Under::Curly => self.out.push_str(";4"),
        }
        let underlined = style.under != Under::None;
        if let (Some(colour), true, true) = (style.under_colour, self.caps.underline_colour, underlined) {
            self.out.str(";58;5;").int(colour);
        }
        self.out.push_str("m");
    }

    /// `text`, already safe, `width` columns wide, in `style`.
    pub(crate) fn put(&mut self, style: Style, text: &str, width: u32) {
        self.style(style);
        self.out.push_str(text);
        self.cols += width;
    }

    /// `text` from anywhere (a path, a label, a name): each control a stand-in.
    pub(crate) fn safe(&mut self, style: Style, text: &str) {
        self.style(style);
        for cluster in blaze_unicode::graphemes(text) {
            if !cluster.contains('\t') && !cluster.chars().any(needs_escape) {
                self.out.push_str(cluster);
                self.cols += blaze_unicode::str_width(cluster) as u32;
                continue;
            }
            for c in cluster.chars() {
                if c == '\t' {
                    self.out.push(' ');
                    self.cols += 1;
                } else if needs_escape(c) {
                    let (buf, len, width) = escape(c, self.caps.unicode);
                    self.out.push_str(core::str::from_utf8(&buf[..usize::from(len)]).unwrap_or("?"));
                    self.cols += width;
                } else {
                    self.out.push(c);
                    self.cols += blaze_unicode::char_width(c).unwrap_or(1) as u32;
                }
            }
        }
    }

    /// `n` blank columns: in the style in force when it draws a space as nothing (only weight
    /// and colour of the glyph), else plain: no escape is written for nothing to see.
    pub(crate) fn spaces(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        let now = self.now;
        if now.under != Under::None || now.inverse || now.strike {
            self.style(Style::PLAIN);
        }
        self.out.repeat(' ', n as usize);
        self.cols += n;
    }

    /// The end of the line: styles reset, a line feed.
    pub(crate) fn end(&mut self) {
        self.style(Style::PLAIN);
        self.out.push_str("\n");
        self.cols = 0;
    }
}
