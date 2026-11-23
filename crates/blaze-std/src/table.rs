//! ANSI-width-aware terminal word wrapping and box-drawn table rendering.
//!
//! Pure `#![no_std]` with zero `unsafe`, zero integer `as` casts, zero `.clone()`, and zero
//! panicking slice indexing, built directly on [`crate::ansi`]:
//!
//! - [`wrap_ansi_into`] / [`wrap_ansi_in`]: word-wraps text to a terminal display column limit
//!   while tracking active SGR styles (closing with `\x1b[0m` before each wrap break and
//!   re-opening the active style on the next line), respecting CJK wide characters and never
//!   splitting inside an ANSI escape sequence.
//! - [`render_table_into`] / [`render_table_in`]: formats Unicode, Rounded, ASCII, Markdown, and
//!   Plain tables aligned by [`crate::ansi::visible_width`] so styled cells align to the exact
//!   terminal column, with ragged-row handling and optional ellipsis truncation.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String};

use crate::ansi::{Color, Segment, Style, char_width, segments, visible_width};

/// Bridge writing `fmt::Write` output into a [`blaze_own::String`] with fallible allocation.
struct TryStringWriter<'a, A: Allocator> {
    out: &'a mut String<A>,
    err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self.out.try_push_str(s) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.err = Some(why);
                Err(fmt::Error)
            }
        }
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        match self.out.try_push(ch) {
            Ok(()) => Ok(()),
            Err(why) => {
                self.err = Some(why);
                Err(fmt::Error)
            }
        }
    }
}

/// Map a standard 0..=7 ANSI color offset (normal or bright) to [`Color`].
const fn indexed_basic_color(offset: u8, bright: bool) -> Option<Color> {
    match (offset, bright) {
        (0, false) => Some(Color::Black),
        (1, false) => Some(Color::Red),
        (2, false) => Some(Color::Green),
        (3, false) => Some(Color::Yellow),
        (4, false) => Some(Color::Blue),
        (5, false) => Some(Color::Magenta),
        (6, false) => Some(Color::Cyan),
        (7, false) => Some(Color::White),
        (0, true) => Some(Color::BrightBlack),
        (1, true) => Some(Color::BrightRed),
        (2, true) => Some(Color::BrightGreen),
        (3, true) => Some(Color::BrightYellow),
        (4, true) => Some(Color::BrightBlue),
        (5, true) => Some(Color::BrightMagenta),
        (6, true) => Some(Color::BrightCyan),
        (7, true) => Some(Color::BrightWhite),
        _ => None,
    }
}

/// Parse an ASCII decimal `u8` SGR parameter; an empty parameter means `0`.
fn parse_sgr_param(part: &str) -> Option<u8> {
    if part.is_empty() {
        return Some(0);
    }
    let mut val = 0u16;
    for &b in part.as_bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u16::from(b - b'0'))?;
        if val > u16::from(u8::MAX) {
            return None;
        }
    }
    u8::try_from(val).ok()
}

/// Update `style` according to an ANSI escape sequence (only CSI SGR `ESC [ ... m` modifies it).
fn apply_sgr(style: &mut Style, esc: &str) {
    let Some(rest) = esc.strip_prefix("\x1b[") else {
        return;
    };
    let Some(params) = rest.strip_suffix('m') else {
        return;
    };
    if params.is_empty() {
        *style = Style::PLAIN;
        return;
    }
    let mut iter = params.split(';');
    while let Some(part) = iter.next() {
        let Some(code) = parse_sgr_param(part) else {
            continue;
        };
        match code {
            0 => *style = Style::PLAIN,
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            7 => style.inverse = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            27 => style.inverse = false,
            29 => style.strikethrough = false,
            30..=37 => {
                style.fg = indexed_basic_color(code - 30, false);
            }
            38 => {
                if let Some(color) = parse_extended_color(&mut iter) {
                    style.fg = Some(color);
                }
            }
            39 => style.fg = None,
            40..=47 => {
                style.bg = indexed_basic_color(code - 40, false);
            }
            48 => {
                if let Some(color) = parse_extended_color(&mut iter) {
                    style.bg = Some(color);
                }
            }
            49 => style.bg = None,
            90..=97 => {
                style.fg = indexed_basic_color(code - 90, true);
            }
            100..=107 => {
                style.bg = indexed_basic_color(code - 100, true);
            }
            _ => {}
        }
    }
}

fn parse_extended_color<'a, I: Iterator<Item = &'a str>>(iter: &mut I) -> Option<Color> {
    let mode = parse_sgr_param(iter.next()?)?;
    match mode {
        5 => {
            let idx = parse_sgr_param(iter.next()?)?;
            Some(Color::Indexed(idx))
        }
        2 => {
            let r = parse_sgr_param(iter.next()?)?;
            let g = parse_sgr_param(iter.next()?)?;
            let b = parse_sgr_param(iter.next()?)?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

fn write_color_sgr<W: fmt::Write>(color: Color, bg: bool, out: &mut W) -> fmt::Result {
    let base = if bg { 10u8 } else { 0u8 };
    match color {
        Color::Black => write!(out, "{}", 30 + base),
        Color::Red => write!(out, "{}", 31 + base),
        Color::Green => write!(out, "{}", 32 + base),
        Color::Yellow => write!(out, "{}", 33 + base),
        Color::Blue => write!(out, "{}", 34 + base),
        Color::Magenta => write!(out, "{}", 35 + base),
        Color::Cyan => write!(out, "{}", 36 + base),
        Color::White => write!(out, "{}", 37 + base),
        Color::BrightBlack => write!(out, "{}", 90 + base),
        Color::BrightRed => write!(out, "{}", 91 + base),
        Color::BrightGreen => write!(out, "{}", 92 + base),
        Color::BrightYellow => write!(out, "{}", 93 + base),
        Color::BrightBlue => write!(out, "{}", 94 + base),
        Color::BrightMagenta => write!(out, "{}", 95 + base),
        Color::BrightCyan => write!(out, "{}", 96 + base),
        Color::BrightWhite => write!(out, "{}", 97 + base),
        Color::Indexed(n) => write!(out, "{};5;{n}", 38 + base),
        Color::Rgb(r, g, b) => write!(out, "{};2;{r};{g};{b}", 38 + base),
    }
}

/// Emit the opening SGR sequence for `style` (without a trailing `\x1b[0m`).
fn write_sgr_open<W: fmt::Write>(style: Style, out: &mut W) -> fmt::Result {
    if style.is_plain() {
        return Ok(());
    }
    out.write_str("\x1b[")?;
    let mut first = true;
    let mut sep = |w: &mut W| -> fmt::Result {
        if first {
            first = false;
            Ok(())
        } else {
            w.write_char(';')
        }
    };
    if style.bold {
        sep(out)?;
        out.write_char('1')?;
    }
    if style.dim {
        sep(out)?;
        out.write_char('2')?;
    }
    if style.italic {
        sep(out)?;
        out.write_char('3')?;
    }
    if style.underline {
        sep(out)?;
        out.write_char('4')?;
    }
    if style.inverse {
        sep(out)?;
        out.write_char('7')?;
    }
    if style.strikethrough {
        sep(out)?;
        out.write_char('9')?;
    }
    if let Some(fg) = style.fg {
        sep(out)?;
        write_color_sgr(fg, false, out)?;
    }
    if let Some(bg) = style.bg {
        sep(out)?;
        write_color_sgr(bg, true, out)?;
    }
    out.write_char('m')
}

/// Advance `pos` past any consecutive ANSI escape sequences starting at `pos` in `input`.
fn skip_escapes(input: &str, mut pos: usize) -> usize {
    while let Some(tail) = input.get(pos..) {
        match segments(tail).next() {
            Some(Segment::Escape(esc)) if !esc.is_empty() => {
                pos += esc.len();
            }
            Some(Segment::Escape(_) | Segment::Text(_)) | None => break,
        }
    }
    pos
}

/// Token yielded by the zero-alloc ANSI-aware word tokenizer.
enum WrapToken<'a> {
    Newline,
    Spaces(&'a str),
    Word(&'a str),
    Escapes(&'a str),
}

struct WrapTokenizer<'a> {
    rest: &'a str,
}

impl<'a> Iterator for WrapTokenizer<'a> {
    type Item = WrapToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.starts_with('\n') {
            self.rest = self.rest.get(1..).unwrap_or("");
            return Some(WrapToken::Newline);
        }
        if self.rest.starts_with(' ') {
            let mut pos = 0usize;
            while let Some(tail) = self.rest.get(pos..) {
                if tail.is_empty() {
                    break;
                }
                match segments(tail).next() {
                    Some(Segment::Text(txt)) => {
                        let sp = txt.as_bytes().iter().take_while(|&&b| b == b' ').count();
                        if sp == 0 {
                            break;
                        }
                        pos += sp;
                        if sp < txt.len() {
                            break;
                        }
                    }
                    Some(Segment::Escape(_)) => {
                        let after = skip_escapes(self.rest, pos);
                        let next_byte = self.rest.as_bytes().get(after).copied();
                        match next_byte {
                            Some(b' ') => {
                                pos = after;
                            }
                            None | Some(b'\n') => {
                                pos = after;
                                break;
                            }
                            Some(_) => {
                                // Escapes precede a visible word; attach them to that word.
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            let (tok, tail) = self.rest.split_at_checked(pos).unwrap_or((self.rest, ""));
            self.rest = tail;
            return Some(WrapToken::Spaces(tok));
        }
        if self.rest.starts_with('\x1b') {
            let after = skip_escapes(self.rest, 0);
            let next_byte = self.rest.as_bytes().get(after).copied();
            if matches!(next_byte, None | Some(b' ' | b'\n')) && after > 0 {
                let (tok, tail) = self.rest.split_at_checked(after).unwrap_or((self.rest, ""));
                self.rest = tail;
                return Some(WrapToken::Escapes(tok));
            }
        }
        // Word token: starts with a non-space/non-newline char or escapes preceding one.
        let mut pos = 0usize;
        while let Some(tail) = self.rest.get(pos..) {
            if tail.is_empty() {
                break;
            }
            match segments(tail).next() {
                Some(Segment::Text(txt)) => {
                    let wlen = txt
                        .as_bytes()
                        .iter()
                        .position(|&b| b == b' ' || b == b'\n')
                        .unwrap_or(txt.len());
                    if wlen == 0 {
                        break;
                    }
                    pos += wlen;
                    if wlen < txt.len() {
                        break;
                    }
                }
                Some(Segment::Escape(esc)) => {
                    if esc.is_empty() {
                        break;
                    }
                    pos += esc.len();
                }
                None => break,
            }
        }
        let (tok, tail) = self.rest.split_at_checked(pos).unwrap_or((self.rest, ""));
        self.rest = tail;
        Some(WrapToken::Word(tok))
    }
}

/// Apply all ANSI escapes inside `slice` to `style`, emitting non-SGR escapes only if requested.
fn apply_escapes_in(slice: &str, style: &mut Style) {
    for seg in segments(slice) {
        if let Segment::Escape(esc) = seg {
            apply_sgr(style, esc);
        }
    }
}

/// Emit a slice verbatim while updating `style` on every ANSI escape segment.
fn emit_slice_updating<W: fmt::Write>(
    slice: &str,
    style: &mut Style,
    out: &mut W,
) -> fmt::Result {
    for seg in segments(slice) {
        match seg {
            Segment::Escape(esc) => {
                apply_sgr(style, esc);
                out.write_str(esc)?;
            }
            Segment::Text(txt) => {
                out.write_str(txt)?;
            }
        }
    }
    Ok(())
}

/// Emit a wrap line break: close active SGR with `\x1b[0m`, write `\n`, update style from any
/// dropped separator slice, and re-open the active SGR on the new line.
fn emit_wrap_break<W: fmt::Write>(
    style: &mut Style,
    dropped_sep: Option<&str>,
    out: &mut W,
    plant_skip_reopen: bool,
) -> fmt::Result {
    if !style.is_plain() {
        out.write_str("\x1b[0m")?;
    }
    out.write_char('\n')?;
    if let Some(sep) = dropped_sep {
        apply_escapes_in(sep, style);
    }
    if !plant_skip_reopen && !style.is_plain() {
        write_sgr_open(*style, out)?;
    }
    Ok(())
}

/// Emit a single word token, breaking across lines character-by-character if wider than `max_width`.
fn emit_word_wrapped<W: fmt::Write>(
    word: &str,
    word_width: usize,
    max_width: usize,
    col: &mut usize,
    style: &mut Style,
    out: &mut W,
    plant_skip_reopen: bool,
) -> fmt::Result {
    if *col + word_width <= max_width {
        emit_slice_updating(word, style, out)?;
        *col += word_width;
        return Ok(());
    }
    for seg in segments(word) {
        match seg {
            Segment::Escape(esc) => {
                apply_sgr(style, esc);
                out.write_str(esc)?;
            }
            Segment::Text(txt) => {
                for ch in txt.chars() {
                    let cw = char_width(ch);
                    if *col > 0 && *col + cw > max_width {
                        emit_wrap_break(style, None, out, plant_skip_reopen)?;
                        *col = 0;
                    }
                    out.write_char(ch)?;
                    *col += cw;
                }
            }
        }
    }
    Ok(())
}

/// Word-wrap `input` to at most `max_width` visible terminal columns, writing into `out`.
///
/// Tracks active SGR escape sequences across line breaks: before a wrap break, emits `\x1b[0m`
/// when a style is active, and re-opens the active SGR style at the start of the next line.
/// Preserves explicit `\n`, respects CJK/emoji display widths, and never splits inside an ANSI
/// escape sequence.
pub fn wrap_ansi_into<W: fmt::Write>(
    input: &str,
    max_width: usize,
    out: &mut W,
) -> fmt::Result {
    wrap_ansi_into_planted(input, max_width, out, false)
}

fn wrap_ansi_into_planted<W: fmt::Write>(
    input: &str,
    max_width: usize,
    out: &mut W,
    plant_skip_reopen: bool,
) -> fmt::Result {
    if max_width == 0 || input.is_empty() {
        return out.write_str(input);
    }

    let mut style = Style::PLAIN;
    let mut col = 0usize;
    let mut pending_spaces: Option<&str> = None;

    for token in (WrapTokenizer { rest: input }) {
        match token {
            WrapToken::Newline => {
                if let Some(sp) = pending_spaces.take() {
                    let sw = visible_width(sp);
                    if col + sw <= max_width {
                        emit_slice_updating(sp, &mut style, out)?;
                        emit_wrap_break(&mut style, None, out, plant_skip_reopen)?;
                    } else {
                        emit_wrap_break(&mut style, Some(sp), out, plant_skip_reopen)?;
                    }
                } else {
                    emit_wrap_break(&mut style, None, out, plant_skip_reopen)?;
                }
                col = 0;
            }
            WrapToken::Escapes(esc) => {
                emit_slice_updating(esc, &mut style, out)?;
            }
            WrapToken::Spaces(sp) => {
                pending_spaces = Some(sp);
            }
            WrapToken::Word(word) => {
                let ww = visible_width(word);
                if let Some(sp) = pending_spaces.take() {
                    let sw = visible_width(sp);
                    if col > 0 {
                        if col + sw + ww <= max_width {
                            emit_slice_updating(sp, &mut style, out)?;
                            col += sw;
                            emit_slice_updating(word, &mut style, out)?;
                            col += ww;
                        } else {
                            emit_wrap_break(&mut style, Some(sp), out, plant_skip_reopen)?;
                            col = 0;
                            emit_word_wrapped(
                                word,
                                ww,
                                max_width,
                                &mut col,
                                &mut style,
                                out,
                                plant_skip_reopen,
                            )?;
                        }
                    } else if sw + ww <= max_width {
                        emit_slice_updating(sp, &mut style, out)?;
                        col += sw;
                        emit_slice_updating(word, &mut style, out)?;
                        col += ww;
                    } else {
                        let was_plain = style.is_plain();
                        apply_escapes_in(sp, &mut style);
                        if !was_plain && style.is_plain() {
                            out.write_str("\x1b[0m")?;
                        } else if !plant_skip_reopen && !style.is_plain() && was_plain {
                            write_sgr_open(style, out)?;
                        }
                        emit_word_wrapped(
                            word,
                            ww,
                            max_width,
                            &mut col,
                            &mut style,
                            out,
                            plant_skip_reopen,
                        )?;
                    }
                } else {
                    if col > 0 && col + ww > max_width {
                        emit_wrap_break(&mut style, None, out, plant_skip_reopen)?;
                        col = 0;
                    }
                    emit_word_wrapped(
                        word,
                        ww,
                        max_width,
                        &mut col,
                        &mut style,
                        out,
                        plant_skip_reopen,
                    )?;
                }
            }
        }
    }

    if let Some(sp) = pending_spaces.take() {
        let sw = visible_width(sp);
        if col + sw <= max_width {
            emit_slice_updating(sp, &mut style, out)?;
        } else {
            let was_plain = style.is_plain();
            apply_escapes_in(sp, &mut style);
            if !was_plain && style.is_plain() {
                out.write_str("\x1b[0m")?;
            }
        }
    }

    Ok(())
}

/// Word-wrap `input` to at most `max_width` visible terminal columns, allocating in `alloc`.
pub fn wrap_ansi_in<A: Allocator + Copy>(
    input: &str,
    max_width: usize,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    let mut writer = TryStringWriter { out: &mut out, err: None };
    if wrap_ansi_into(input, max_width, &mut writer).is_err()
        && let Some(why) = writer.err
    {
        return Err(why);
    }
    Ok(out)
}

/// Visual border style for rendered tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxStyle {
    /// Square Unicode box-drawing borders (`┌─┬┐`, `│`, `├─┼┤`, `└─┴┘`).
    Unicode,
    /// Rounded Unicode box-drawing borders (`╭─┬╮`, `│`, `├─┼┤`, `╰─┴╯`).
    Rounded,
    /// Portable ASCII borders (`+-++`, `|`, `+-++`, `+-++`).
    Ascii,
    /// GitHub-Flavored Markdown pipe table (`| col |` and `| :--- |`).
    Markdown,
    /// Borderless space-separated columns.
    Plain,
}

/// Horizontal text alignment within a table column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Formatting options for [`render_table_into`] and [`render_table_in`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableOpts {
    /// Border style to draw around and between cells.
    pub style: BoxStyle,
    /// Number of spaces printed on each side of a cell's content (between columns for `Plain`).
    pub padding: usize,
    /// Optional maximum visible width per column; wider cells are truncated with `…`.
    pub max_col_width: Option<usize>,
}

impl TableOpts {
    /// Construct table options for `style` with 1-space padding and no column width cap.
    #[must_use]
    pub const fn new(style: BoxStyle) -> Self {
        Self { style, padding: 1, max_col_width: None }
    }
}

impl Default for TableOpts {
    fn default() -> Self {
        Self::new(BoxStyle::Unicode)
    }
}

#[derive(Clone, Copy)]
struct BoxChars {
    top_left: char,
    top_mid: char,
    top_right: char,
    mid_left: char,
    mid_mid: char,
    mid_right: char,
    bot_left: char,
    bot_mid: char,
    bot_right: char,
    horiz: char,
}

const UNICODE_BOX: BoxChars = BoxChars {
    top_left: '┌',
    top_mid: '┬',
    top_right: '┐',
    mid_left: '├',
    mid_mid: '┼',
    mid_right: '┤',
    bot_left: '└',
    bot_mid: '┴',
    bot_right: '┘',
    horiz: '─',
};

const ROUNDED_BOX: BoxChars = BoxChars {
    top_left: '╭',
    top_mid: '┬',
    top_right: '╮',
    mid_left: '├',
    mid_mid: '┼',
    mid_right: '┤',
    bot_left: '╰',
    bot_mid: '┴',
    bot_right: '╯',
    horiz: '─',
};

const ASCII_BOX: BoxChars = BoxChars {
    top_left: '+',
    top_mid: '+',
    top_right: '+',
    mid_left: '+',
    mid_mid: '+',
    mid_right: '+',
    bot_left: '+',
    bot_mid: '+',
    bot_right: '+',
    horiz: '-',
};

/// Visible width of a single table cell (newlines are sanitized to single spaces when rendered).
fn cell_visible_width(cell: &str, plant_use_byte_len: bool) -> usize {
    if plant_use_byte_len {
        return cell.len();
    }
    let mut width = 0usize;
    for seg in segments(cell) {
        if let Segment::Text(txt) = seg {
            for ch in txt.chars() {
                if ch == '\n' {
                    width += 1;
                } else if ch != '\r' {
                    width += char_width(ch);
                }
            }
        }
    }
    width
}

const INLINE_COL_CACHE: usize = 64;

fn compute_col_width<H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    col: usize,
    headers: &[H],
    rows: &[R],
    opts: TableOpts,
    plant_use_byte_len: bool,
) -> usize {
    let mut w = 0usize;
    if let Some(h) = headers.get(col) {
        let hw = cell_visible_width(h.as_ref(), plant_use_byte_len);
        if hw > w {
            w = hw;
        }
    }
    for row in rows {
        if let Some(c) = row.as_ref().get(col) {
            let cw = cell_visible_width(c.as_ref(), plant_use_byte_len);
            if cw > w {
                w = cw;
            }
        }
    }
    if matches!(opts.style, BoxStyle::Markdown) && w < 3 {
        w = 3;
    }
    if let Some(cap) = opts.max_col_width
        && w > cap
    {
        w = cap;
    }
    w
}

fn col_width_at<H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    col: usize,
    cached: &[usize; INLINE_COL_CACHE],
    headers: &[H],
    rows: &[R],
    opts: TableOpts,
    plant_use_byte_len: bool,
) -> usize {
    match cached.get(col) {
        Some(&w) => w,
        None => compute_col_width(col, headers, rows, opts, plant_use_byte_len),
    }
}

fn write_repeat<W: fmt::Write>(ch: char, count: usize, out: &mut W) -> fmt::Result {
    for _ in 0..count {
        out.write_char(ch)?;
    }
    Ok(())
}

/// Write a cell (sanitizing newlines and optionally truncating to `max_w` with `…`), returning
/// its rendered visible width and closing any open SGR style so colors never bleed into borders.
fn write_cell_content<W: fmt::Write>(
    cell: &str,
    max_w: usize,
    plant_use_byte_len: bool,
    out: &mut W,
) -> Result<usize, fmt::Error> {
    let raw_w = cell_visible_width(cell, plant_use_byte_len);
    let mut style = Style::PLAIN;
    if raw_w <= max_w {
        for seg in segments(cell) {
            match seg {
                Segment::Escape(esc) => {
                    apply_sgr(&mut style, esc);
                    out.write_str(esc)?;
                }
                Segment::Text(txt) => {
                    for ch in txt.chars() {
                        if ch == '\n' {
                            out.write_char(' ')?;
                        } else if ch != '\r' {
                            out.write_char(ch)?;
                        }
                    }
                }
            }
        }
        if !style.is_plain() {
            out.write_str("\x1b[0m")?;
        }
        return Ok(raw_w);
    }

    if max_w == 0 {
        return Ok(0);
    }
    if max_w == 1 {
        out.write_char('…')?;
        return Ok(1);
    }

    let budget = max_w - 1;
    let mut emitted = 0usize;
    let mut stopped = false;
    for seg in segments(cell) {
        if stopped {
            break;
        }
        match seg {
            Segment::Escape(esc) => {
                apply_sgr(&mut style, esc);
                out.write_str(esc)?;
            }
            Segment::Text(txt) => {
                for ch in txt.chars() {
                    if ch == '\r' {
                        continue;
                    }
                    let rendered = if ch == '\n' { ' ' } else { ch };
                    let cw = if plant_use_byte_len {
                        rendered.len_utf8()
                    } else {
                        char_width(rendered)
                    };
                    if emitted + cw > budget {
                        stopped = true;
                        break;
                    }
                    out.write_char(rendered)?;
                    emitted += cw;
                }
            }
        }
    }
    out.write_char('…')?;
    emitted += 1;
    if !style.is_plain() {
        out.write_str("\x1b[0m")?;
    }
    Ok(emitted)
}

fn write_aligned_cell<W: fmt::Write>(
    cell: &str,
    col_w: usize,
    align: Align,
    plant_use_byte_len: bool,
    out: &mut W,
) -> fmt::Result {
    let raw_w = cell_visible_width(cell, plant_use_byte_len);
    // Compute exact rendered width ahead of time so Left/Center/Right padding is exact even when
    // truncation stops one column early before a wide character.
    let rendered_w = if raw_w <= col_w {
        raw_w
    } else if col_w <= 1 {
        col_w
    } else {
        let budget = col_w - 1;
        let mut acc = 0usize;
        for seg in segments(cell) {
            if let Segment::Text(txt) = seg {
                let mut done = false;
                for ch in txt.chars() {
                    if ch == '\r' {
                        continue;
                    }
                    let rendered = if ch == '\n' { ' ' } else { ch };
                    let cw = if plant_use_byte_len {
                        rendered.len_utf8()
                    } else {
                        char_width(rendered)
                    };
                    if acc + cw > budget {
                        done = true;
                        break;
                    }
                    acc += cw;
                }
                if done {
                    break;
                }
            }
        }
        acc + 1
    };

    let pad = col_w.saturating_sub(rendered_w);
    let (left_pad, right_pad) = match align {
        Align::Left => (0, pad),
        Align::Right => (pad, 0),
        Align::Center => {
            let l = pad / 2;
            (l, pad - l)
        }
    };
    write_repeat(' ', left_pad, out)?;
    let _actual = write_cell_content(cell, col_w, plant_use_byte_len, out)?;
    write_repeat(' ', right_pad, out)
}

fn write_rule<W: fmt::Write, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    left: char,
    horiz: char,
    mid: char,
    right: char,
    num_cols: usize,
    cached: &[usize; INLINE_COL_CACHE],
    headers: &[H],
    rows: &[R],
    opts: TableOpts,
    plant_use_byte_len: bool,
    out: &mut W,
) -> fmt::Result {
    out.write_char(left)?;
    for col in 0..num_cols {
        if col > 0 {
            out.write_char(mid)?;
        }
        let w = col_width_at(col, cached, headers, rows, opts, plant_use_byte_len);
        write_repeat(horiz, w + opts.padding.saturating_mul(2), out)?;
    }
    out.write_char(right)
}

fn write_markdown_sep<W: fmt::Write, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    num_cols: usize,
    cached: &[usize; INLINE_COL_CACHE],
    headers: &[H],
    rows: &[R],
    aligns: &[Align],
    opts: TableOpts,
    plant_use_byte_len: bool,
    out: &mut W,
) -> fmt::Result {
    out.write_char('|')?;
    for col in 0..num_cols {
        let w = col_width_at(col, cached, headers, rows, opts, plant_use_byte_len);
        let align = aligns.get(col).copied().unwrap_or(Align::Left);
        write_repeat(' ', opts.padding, out)?;
        match align {
            Align::Left => {
                if w >= 2 {
                    out.write_char(':')?;
                    write_repeat('-', w - 1, out)?;
                } else {
                    write_repeat('-', w, out)?;
                }
            }
            Align::Center => {
                if w >= 3 {
                    out.write_char(':')?;
                    write_repeat('-', w - 2, out)?;
                    out.write_char(':')?;
                } else if w == 2 {
                    out.write_str("::")?;
                } else {
                    write_repeat('-', w, out)?;
                }
            }
            Align::Right => {
                if w >= 2 {
                    write_repeat('-', w - 1, out)?;
                    out.write_char(':')?;
                } else {
                    write_repeat('-', w, out)?;
                }
            }
        }
        write_repeat(' ', opts.padding, out)?;
        out.write_char('|')?;
    }
    Ok(())
}

fn write_data_row<W: fmt::Write, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>, Cell: AsRef<str>>(
    cells: &[Cell],
    num_cols: usize,
    cached: &[usize; INLINE_COL_CACHE],
    headers: &[H],
    rows: &[R],
    aligns: &[Align],
    opts: TableOpts,
    plant_use_byte_len: bool,
    out: &mut W,
) -> fmt::Result {
    let vert = match opts.style {
        BoxStyle::Unicode | BoxStyle::Rounded => Some('│'),
        BoxStyle::Ascii | BoxStyle::Markdown => Some('|'),
        BoxStyle::Plain => None,
    };
    if let Some(v) = vert {
        out.write_char(v)?;
    }
    let plain_gap = opts.padding.max(1);
    for col in 0..num_cols {
        let w = col_width_at(col, cached, headers, rows, opts, plant_use_byte_len);
        let align = aligns.get(col).copied().unwrap_or(Align::Left);
        let text = cells.get(col).map_or("", AsRef::as_ref);
        if let Some(v) = vert {
            write_repeat(' ', opts.padding, out)?;
            write_aligned_cell(text, w, align, plant_use_byte_len, out)?;
            write_repeat(' ', opts.padding, out)?;
            out.write_char(v)?;
        } else {
            if col > 0 {
                write_repeat(' ', plain_gap, out)?;
            }
            write_aligned_cell(text, w, align, plant_use_byte_len, out)?;
        }
    }
    Ok(())
}

/// Render a table with ANSI-aware column widths into `out`.
///
/// Handles ragged rows (missing cells default to `""`), missing alignments (default to
/// [`Align::Left`]), ANSI SGR escape sequences, CJK wide characters, and optional truncation via
/// `opts.max_col_width`.
pub fn render_table_into<W: fmt::Write, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    headers: &[H],
    rows: &[R],
    aligns: &[Align],
    opts: TableOpts,
    out: &mut W,
) -> fmt::Result {
    render_table_into_planted(headers, rows, aligns, opts, out, false)
}

fn render_table_into_planted<W: fmt::Write, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    headers: &[H],
    rows: &[R],
    aligns: &[Align],
    opts: TableOpts,
    out: &mut W,
    plant_use_byte_len: bool,
) -> fmt::Result {
    let mut num_cols = headers.len();
    for row in rows {
        let rlen = row.as_ref().len();
        if rlen > num_cols {
            num_cols = rlen;
        }
    }
    if num_cols == 0 {
        return Ok(());
    }

    let mut cached = [0usize; INLINE_COL_CACHE];
    let cached_cols = num_cols.min(INLINE_COL_CACHE);
    for col in 0..cached_cols {
        if let Some(slot) = cached.get_mut(col) {
            *slot = compute_col_width(col, headers, rows, opts, plant_use_byte_len);
        }
    }

    let box_chars = match opts.style {
        BoxStyle::Unicode => Some(UNICODE_BOX),
        BoxStyle::Rounded => Some(ROUNDED_BOX),
        BoxStyle::Ascii => Some(ASCII_BOX),
        BoxStyle::Markdown | BoxStyle::Plain => None,
    };

    let mut wrote_any = false;
    let newline_if = |wrote: &mut bool, w: &mut W| -> fmt::Result {
        if *wrote {
            w.write_char('\n')?;
        }
        *wrote = true;
        Ok(())
    };

    if let Some(bc) = box_chars {
        newline_if(&mut wrote_any, out)?;
        write_rule(
            bc.top_left,
            bc.horiz,
            bc.top_mid,
            bc.top_right,
            num_cols,
            &cached,
            headers,
            rows,
            opts,
            plant_use_byte_len,
            out,
        )?;
    }

    if !headers.is_empty() {
        newline_if(&mut wrote_any, out)?;
        write_data_row(
            headers,
            num_cols,
            &cached,
            headers,
            rows,
            aligns,
            opts,
            plant_use_byte_len,
            out,
        )?;
        match opts.style {
            BoxStyle::Unicode | BoxStyle::Rounded | BoxStyle::Ascii => {
                if let Some(bc) = box_chars
                    && !rows.is_empty()
                {
                    newline_if(&mut wrote_any, out)?;
                    write_rule(
                        bc.mid_left,
                        bc.horiz,
                        bc.mid_mid,
                        bc.mid_right,
                        num_cols,
                        &cached,
                        headers,
                        rows,
                        opts,
                        plant_use_byte_len,
                        out,
                    )?;
                }
            }
            BoxStyle::Markdown => {
                newline_if(&mut wrote_any, out)?;
                write_markdown_sep(
                    num_cols,
                    &cached,
                    headers,
                    rows,
                    aligns,
                    opts,
                    plant_use_byte_len,
                    out,
                )?;
            }
            BoxStyle::Plain => {}
        }
    }

    for row in rows {
        newline_if(&mut wrote_any, out)?;
        write_data_row(
            row.as_ref(),
            num_cols,
            &cached,
            headers,
            rows,
            aligns,
            opts,
            plant_use_byte_len,
            out,
        )?;
    }

    if let Some(bc) = box_chars {
        newline_if(&mut wrote_any, out)?;
        write_rule(
            bc.bot_left,
            bc.horiz,
            bc.bot_mid,
            bc.bot_right,
            num_cols,
            &cached,
            headers,
            rows,
            opts,
            plant_use_byte_len,
            out,
        )?;
    }

    Ok(())
}

/// Render a table with ANSI-aware column widths into a newly allocated [`blaze_own::String`].
pub fn render_table_in<A: Allocator + Copy, H: AsRef<str>, R: AsRef<[C]>, C: AsRef<str>>(
    headers: &[H],
    rows: &[R],
    aligns: &[Align],
    opts: TableOpts,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::new_in(alloc);
    let mut writer = TryStringWriter { out: &mut out, err: None };
    if render_table_into(headers, rows, aligns, opts, &mut writer).is_err()
        && let Some(why) = writer.err
    {
        return Err(why);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use crate::ansi::{Color, Style, strip_in, visible_width};
    use crate::table::{
        Align, BoxStyle, TableOpts, render_table_in, render_table_into_planted, wrap_ansi_in,
        wrap_ansi_into_planted,
    };

    #[test]
    fn wrap_preserves_ansi_style_across_lines() {
        let h = heap();
        let input = "\x1b[1;31mhello world again\x1b[0m";
        let wrapped = wrap_ansi_in(input, 7, h.alloc()).expect("wrap");
        assert_eq!(
            wrapped.as_str(),
            "\x1b[1;31mhello\x1b[0m\n\x1b[1;31mworld\x1b[0m\n\x1b[1;31magain\x1b[0m"
        );
        let stripped = strip_in(wrapped.as_str(), h.alloc()).expect("strip");
        assert_eq!(stripped.as_str(), "hello\nworld\nagain");
    }

    #[test]
    fn wrap_respects_cjk_wide_and_explicit_newlines() {
        let h = heap();
        let input = "日本語テスト\nhello world";
        let wrapped = wrap_ansi_in(input, 6, h.alloc()).expect("wrap");
        assert_eq!(wrapped.as_str(), "日本語\nテスト\nhello\nworld");
        for line in wrapped.as_str().split('\n') {
            assert!(visible_width(line) <= 6, "line too wide: {line:?}");
        }
    }

    #[test]
    fn table_all_styles_and_alignments() {
        let h = heap();
        let headers = ["Name", "Count", "Status"];
        let rows = [["alpha", "42", "ok"], ["beta", "1000", "fail"]];
        let aligns = [Align::Left, Align::Right, Align::Center];

        for style in [
            BoxStyle::Unicode,
            BoxStyle::Rounded,
            BoxStyle::Ascii,
            BoxStyle::Markdown,
            BoxStyle::Plain,
        ] {
            let rendered =
                render_table_in(&headers, &rows, &aligns, TableOpts::new(style), h.alloc())
                    .expect("render");
            let mut lines = rendered.as_str().split('\n');
            let first_w = visible_width(lines.next().expect("first line"));
            assert!(first_w > 0);
            for line in lines {
                assert_eq!(
                    visible_width(line),
                    first_w,
                    "style {style:?} uneven line: {line:?}"
                );
            }
        }

        let md = render_table_in(
            &headers,
            &rows,
            &aligns,
            TableOpts::new(BoxStyle::Markdown),
            h.alloc(),
        )
        .expect("md");
        assert!(md.as_str().contains("| :---- | ----: | :----: |"));
    }

    #[test]
    fn table_colored_cells_align_identically_to_plain_and_truncate() {
        let h = heap();
        let plain_headers = ["Crate", "Note"];
        let plain_rows = [["blaze", "日本"], ["bstd", "fast"]];

        let styled_crate = format_in!(h.alloc(), "{}", Style::new().bold().fg(Color::Cyan).paint("blaze"));
        let styled_note = format_in!(h.alloc(), "{}", Style::new().fg(Color::Green).paint("日本"));
        let styled_bstd = format_in!(h.alloc(), "{}", Style::new().fg(Color::Yellow).paint("bstd"));
        let styled_fast = format_in!(h.alloc(), "{}", Style::new().fg(Color::Red).paint("fast"));

        let styled_rows = [
            [styled_crate.as_str(), styled_note.as_str()],
            [styled_bstd.as_str(), styled_fast.as_str()],
        ];

        let opts = TableOpts::new(BoxStyle::Rounded);
        let plain_tbl =
            render_table_in(&plain_headers, &plain_rows, &[], opts, h.alloc()).expect("plain");
        let styled_tbl =
            render_table_in(&plain_headers, &styled_rows, &[], opts, h.alloc()).expect("styled");
        let stripped = strip_in(styled_tbl.as_str(), h.alloc()).expect("strip");
        assert_eq!(stripped.as_str(), plain_tbl.as_str());

        let capped = TableOpts { style: BoxStyle::Unicode, padding: 1, max_col_width: Some(4) };
        let trunc_tbl =
            render_table_in(&plain_headers, &styled_rows, &[], capped, h.alloc()).expect("trunc");
        let stripped_trunc = strip_in(trunc_tbl.as_str(), h.alloc()).expect("strip trunc");
        assert!(stripped_trunc.as_str().contains("bla…"));
        let mut lines = trunc_tbl.as_str().split('\n');
        let w0 = visible_width(lines.next().expect("line 0"));
        for line in lines {
            assert_eq!(visible_width(line), w0);
        }
    }

    #[test]
    fn table_handles_ragged_rows() {
        let h = heap();
        let headers = ["A", "B", "C"];
        let mut rows: OwnVec<OwnVec<&str, _>, _> = OwnVec::new_in(h.alloc());
        let mut r0 = OwnVec::new_in(h.alloc());
        r0.push("only_one");
        rows.push(r0);
        let mut r1 = OwnVec::new_in(h.alloc());
        r1.push("1");
        r1.push("2");
        r1.push("3");
        r1.push("extra");
        rows.push(r1);

        let rendered = render_table_in(
            &headers,
            rows.as_slice(),
            &[Align::Left, Align::Right],
            TableOpts::new(BoxStyle::Ascii),
            h.alloc(),
        )
        .expect("ragged");
        let mut lines = rendered.as_str().split('\n');
        let w0 = visible_width(lines.next().expect("line"));
        for line in lines {
            assert_eq!(visible_width(line), w0);
        }
    }

    /// The invariant: ANSI-styled line wrapping matches plain wrapping and respects visible column width.
    #[test]
    fn property_wrap_ansi_matches_plain_and_respects_width() {
        let h = heap();
        const PIECES: &[&str] = &["hello", "world", " ", "  ", "\n", "日本", "café", "abcdefghij"];
        const STYLES: &[Style] = &[
            Style::PLAIN,
            Style::new().fg(Color::Red),
            Style::new().bold().fg(Color::Cyan),
            Style::new().italic().bg(Color::Blue),
        ];

        blaze_sim::check(Config::cases(256), |tape| {
            let count = usize::try_from(tape.int_in(0..=10, "count")).unwrap_or(0);
            let max_width = usize::try_from(tape.int_in(2..=20, "width")).unwrap_or(8);
            let mut styled = OwnString::new_in(h.alloc());
            for _ in 0..count {
                let piece_idx =
                    usize::try_from(tape.below(u64::try_from(PIECES.len()).unwrap_or(1), "piece"))
                        .unwrap_or(0);
                let style_idx =
                    usize::try_from(tape.below(u64::try_from(STYLES.len()).unwrap_or(1), "style"))
                        .unwrap_or(0);
                let piece = PIECES[piece_idx];
                let st = STYLES[style_idx];
                if st.is_plain() {
                    styled.push_str(piece);
                } else {
                    let painted = format_in!(h.alloc(), "{}", st.paint(piece));
                    styled.push_str(painted.as_str());
                }
            }

            let plain = strip_in(styled.as_str(), h.alloc()).expect("strip input");
            let wrapped_styled = wrap_ansi_in(styled.as_str(), max_width, h.alloc()).expect("wrap");
            let wrapped_plain = wrap_ansi_in(plain.as_str(), max_width, h.alloc()).expect("wrap p");
            let stripped_wrapped =
                strip_in(wrapped_styled.as_str(), h.alloc()).expect("strip wrapped");

            if stripped_wrapped.as_str() != wrapped_plain.as_str() {
                return Err(Broke::Oracle(
                    format_in!(
                        h.alloc(),
                        "strip(wrap(styled)) != wrap(plain): {:?} vs {:?}",
                        stripped_wrapped.as_str(),
                        wrapped_plain.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
            for line in wrapped_styled.as_str().split('\n') {
                let lw = visible_width(line);
                if lw > max_width {
                    return Err(Broke::Oracle(
                        format_in!(h.alloc(), "line width {lw} > max_width {max_width}: {line:?}")
                            .as_str()
                            .into(),
                    ));
                }
            }
            Ok(())
        });
    }

    #[test]
    fn property_table_lines_have_uniform_visible_width() {
        let h = heap();
        const CELLS: &[&str] = &["", "hi", "blaze", "日本", "a\nb", "verylongcellname"];
        const STYLES: &[BoxStyle] =
            &[BoxStyle::Unicode, BoxStyle::Rounded, BoxStyle::Ascii, BoxStyle::Markdown];
        const ALIGNS: &[Align] = &[Align::Left, Align::Center, Align::Right];

        blaze_sim::check(Config::cases(256), |tape| {
            let style =
                STYLES[usize::try_from(tape.below(u64::try_from(STYLES.len()).unwrap_or(1), "st"))
                    .unwrap_or(0)];
            let cols = usize::try_from(tape.int_in(1..=4, "cols")).unwrap_or(1);
            let nrows = usize::try_from(tape.int_in(0..=5, "rows")).unwrap_or(0);
            let pad = usize::try_from(tape.int_in(0..=2, "pad")).unwrap_or(1);
            let cap = if tape.coin("cap") {
                Some(usize::try_from(tape.int_in(2..=10, "cap_w")).unwrap_or(4))
            } else {
                None
            };

            let mut headers: OwnVec<OwnString<_>, _> = OwnVec::new_in(h.alloc());
            let mut aligns: OwnVec<Align, _> = OwnVec::new_in(h.alloc());
            for _ in 0..cols {
                let c = CELLS[usize::try_from(
                    tape.below(u64::try_from(CELLS.len()).unwrap_or(1), "h"),
                )
                .unwrap_or(0)];
                let s = if tape.coin("h_color") {
                    format_in!(h.alloc(), "{}", Style::new().bold().fg(Color::Cyan).paint(c))
                } else {
                    OwnString::from_str_in(c, h.alloc())
                };
                headers.push(s);
                aligns.push(
                    ALIGNS[usize::try_from(
                        tape.below(u64::try_from(ALIGNS.len()).unwrap_or(1), "al"),
                    )
                    .unwrap_or(0)],
                );
            }

            let mut rows: OwnVec<OwnVec<OwnString<_>, _>, _> = OwnVec::new_in(h.alloc());
            for _ in 0..nrows {
                let rcols = usize::try_from(tape.int_in(0..=5, "rcols")).unwrap_or(1);
                let mut row: OwnVec<OwnString<_>, _> = OwnVec::new_in(h.alloc());
                for _ in 0..rcols {
                    let c = CELLS[usize::try_from(
                        tape.below(u64::try_from(CELLS.len()).unwrap_or(1), "c"),
                    )
                    .unwrap_or(0)];
                    let s = if tape.coin("c_color") {
                        format_in!(h.alloc(), "{}", Style::new().fg(Color::Green).paint(c))
                    } else {
                        OwnString::from_str_in(c, h.alloc())
                    };
                    row.push(s);
                }
                rows.push(row);
            }

            let opts = TableOpts { style, padding: pad, max_col_width: cap };
            let rendered = render_table_in(
                headers.as_slice(),
                rows.as_slice(),
                aligns.as_slice(),
                opts,
                h.alloc(),
            )
            .expect("render");
            let mut lines = rendered.as_str().split('\n');
            let first = visible_width(lines.next().expect("line"));
            for line in lines {
                let w = visible_width(line);
                if w != first {
                    return Err(Broke::Oracle(
                        format_in!(h.alloc(), "row width {w} != {first} in {rendered:?}")
                            .as_str()
                            .into(),
                    ));
                }
            }
            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_byte_len_table_misalignment_and_lost_wrap_style() {
        let h = heap();
        // Planted bug 1: using byte length instead of visible_width misaligns colored rows.
        blaze_sim::control(Config::cases(64), false, true, "", |tape, plant| {
            let word = if tape.coin("w") { "alpha" } else { "beta" };
            let colored = format_in!(h.alloc(), "{}", Style::new().fg(Color::Red).paint(word));
            let headers = ["Col"];
            let rows = [[colored.as_str()], ["plain"]];
            let mut out = OwnString::new_in(h.alloc());
            render_table_into_planted(
                &headers,
                &rows,
                &[],
                TableOpts::new(BoxStyle::Unicode),
                &mut out,
                plant,
            )
            .expect("render");
            let mut lines = out.as_str().split('\n');
            let first = visible_width(lines.next().expect("top"));
            for line in lines {
                if visible_width(line) != first {
                    return Err(Broke::Oracle(
                        format_in!(h.alloc(), "misaligned line: {line:?}").as_str().into(),
                    ));
                }
            }
            Ok(())
        });

        // Planted bug 2: forgetting to reopen SGR style on wrapped line breaks style continuity.
        blaze_sim::control(Config::cases(64), false, true, "", |tape, plant| {
            let input = if tape.coin("s") {
                "\x1b[32mhello world\x1b[0m"
            } else {
                "\x1b[1;31malpha beta\x1b[0m"
            };
            let mut out = OwnString::new_in(h.alloc());
            wrap_ansi_into_planted(input, 6, &mut out, plant).expect("wrap");
            let second_line = out.as_str().split('\n').nth(1).unwrap_or("");
            if !second_line.starts_with("\x1b[") {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "second line lost active SGR: {second_line:?}")
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }
}
