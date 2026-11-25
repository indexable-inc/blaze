//! SGR (`CSI … m`) as ghostty's sgr.zig parses it and Screen.zig's `setAttribute` applies it:
//! an iterator turns the parameters into [`Attribute`]s, and [`apply`] edits a [`Style`].
//!
//! Colon groups are allowed only on 4, 38, 48 and 58; on any other code the group is consumed
//! and comes out as [`Attribute::Unknown`], which [`apply`] ignores. An empty list means 0.
//! Values a colour takes are truncated to `u8` as ghostty's `@truncate` does.

use crate::parser::Csi;
use crate::style::{Color, Rgb, Style, Underline};

/// One SGR attribute (sgr.zig:16 `Attribute`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Attribute<'a> {
    /// 0 or an empty list: the default style.
    Unset,
    /// A code ghostty does not act on, or a malformed group. `full` is every parameter, and
    /// `partial` the ones this attribute covers (ghostty logs them).
    Unknown {
        full: &'a [u16],
        partial: &'a [u16],
    },
    Bold,
    /// 22: bold and faint off (they share the code).
    ResetBold,
    Italic,
    ResetItalic,
    Faint,
    /// 4, 4:n, 21 (double), 24 (none).
    Underline(Underline),
    /// 58;2 or 58:2: the underline colour as RGB.
    UnderlineColor(Rgb),
    /// 58;5;n: the underline colour from the palette.
    UnderlineColor256(u8),
    ResetUnderlineColor,
    Overline,
    ResetOverline,
    Blink,
    ResetBlink,
    Inverse,
    ResetInverse,
    Invisible,
    ResetInvisible,
    Strikethrough,
    ResetStrikethrough,
    /// 38;2 / 38:2.
    DirectColorFg(Rgb),
    /// 48;2 / 48:2.
    DirectColorBg(Rgb),
    /// 30–37: palette 0–7.
    Fg8(u8),
    /// 40–47: palette 0–7.
    Bg8(u8),
    /// 90–97: palette 8–15.
    BrightFg8(u8),
    /// 100–107: palette 8–15.
    BrightBg8(u8),
    /// 38;5;n.
    Fg256(u8),
    /// 48;5;n.
    Bg256(u8),
    ResetFg,
    ResetBg,
}

/// The attributes of one SGR sequence, in order (sgr.zig:173 `Parser`).
#[derive(Clone, Debug)]
pub struct Parser<'a> {
    params: &'a [u16],
    /// Bit `i`: parameter `i` is followed by `:` (as [`Csi::colons`]).
    colons: u32,
    idx: usize,
}

impl<'a> Parser<'a> {
    pub fn new(csi: &Csi<'a>) -> Self {
        Self::from_parts(csi.params, csi.colons)
    }

    /// From parameters and a colon mask, bit `i` meaning a `:` follows parameter `i`.
    pub fn from_parts(params: &'a [u16], colons: u32) -> Self {
        Self { params, colons, idx: 0 }
    }

    /// Whether parameter `i` is followed by a colon. Past the parser's 24 slots (or past the
    /// mask's 32 bits) nothing is.
    fn sep(&self, i: usize) -> bool {
        i < crate::parser::MAX_PARAMS && self.colons & (1 << i) != 0
    }

    /// sgr.zig:508 `countColon`: the colons from here, not counting the last parameter's.
    fn count_colon(&self) -> usize {
        let mut i = self.idx;
        let mut n = 0;
        while i < self.params.len().saturating_sub(1) && self.sep(i) {
            n += 1;
            i += 1;
        }
        n
    }

    /// sgr.zig:519 `consumeUnknownColon`.
    fn consume_unknown_colon(&mut self) {
        self.idx += self.count_colon() + 1;
    }

    fn unknown(&self, partial: &'a [u16]) -> Attribute<'a> {
        Attribute::Unknown { full: self.params, partial }
    }

    /// sgr.zig:437 `parseDirectColor`: `None` is a malformed group.
    fn direct(&mut self, slice: &[u16], colon: bool) -> Option<Rgb> {
        if slice.len() < 5 {
            return None;
        }
        let rgb = |s: &[u16]| Rgb { r: s[0] as u8, g: s[1] as u8, b: s[2] as u8 };
        if !colon {
            self.idx += 4;
            return Some(rgb(&slice[2..5]));
        }
        match self.count_colon() {
            3 => {
                self.idx += 4;
                Some(rgb(&slice[2..5]))
            }
            // The colorspace id (`38:2:cs:r:g:b`) is skipped, as xterm does.
            4 => {
                self.idx += 5;
                Some(rgb(&slice[3..6]))
            }
            _ => {
                self.consume_unknown_colon();
                None
            }
        }
    }

    /// 38, 48 or 58 with the sub-parameter in `slice[1]`.
    fn extended(
        &mut self,
        slice: &'a [u16],
        colon: bool,
        rgb: fn(Rgb) -> Attribute<'a>,
        idx: fn(u8) -> Attribute<'a>,
    ) -> Attribute<'a> {
        if slice.len() >= 2 {
            match slice[1] {
                2 => {
                    if let Some(c) = self.direct(slice, colon) {
                        return rgb(c);
                    }
                }
                5 if slice.len() >= 3 => {
                    self.idx += 2;
                    return idx(slice[2] as u8);
                }
                _ => {}
            }
        }
        self.unknown(slice)
    }
}

impl<'a> Iterator for Parser<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Attribute<'a>> {
        if self.idx >= self.params.len() {
            let first = self.idx == 0;
            // One past, so an empty list yields Unset once.
            self.idx = self.idx.saturating_add(1);
            return first.then_some(Attribute::Unset);
        }
        let slice = &self.params[self.idx..];
        let colon = self.sep(self.idx);
        self.idx += 1;

        if colon && !matches!(slice[0], 4 | 38 | 48 | 58) {
            let start = self.idx;
            while self.sep(self.idx) {
                self.idx += 1;
            }
            self.idx += 1;
            let n = (self.idx - start + 1).min(slice.len());
            return Some(self.unknown(&slice[..n]));
        }

        use Attribute as A;
        Some(match slice[0] {
            0 => A::Unset,
            1 => A::Bold,
            2 => A::Faint,
            3 => A::Italic,
            4 if !colon => A::Underline(Underline::Single),
            4 => {
                // A trailing colon with nothing after it (`58:4:m`) leaves a colon on the last
                // parameter: malformed.
                if slice.len() < 2 {
                    return Some(self.unknown(slice));
                }
                if self.sep(self.idx) {
                    self.consume_unknown_colon();
                    return Some(self.unknown(slice));
                }
                self.idx += 1;
                A::Underline(match slice[1] {
                    0 => Underline::None,
                    2 => Underline::Double,
                    3 => Underline::Curly,
                    4 => Underline::Dotted,
                    5 => Underline::Dashed,
                    // 1, and any style ghostty does not know, draws single.
                    _ => Underline::Single,
                })
            }
            5 | 6 => A::Blink,
            7 => A::Inverse,
            8 => A::Invisible,
            9 => A::Strikethrough,
            21 => A::Underline(Underline::Double),
            22 => A::ResetBold,
            23 => A::ResetItalic,
            24 => A::Underline(Underline::None),
            25 => A::ResetBlink,
            27 => A::ResetInverse,
            28 => A::ResetInvisible,
            29 => A::ResetStrikethrough,
            n @ 30..=37 => A::Fg8((n - 30) as u8),
            38 => self.extended(slice, colon, A::DirectColorFg, A::Fg256),
            39 => A::ResetFg,
            n @ 40..=47 => A::Bg8((n - 40) as u8),
            48 => self.extended(slice, colon, A::DirectColorBg, A::Bg256),
            49 => A::ResetBg,
            53 => A::Overline,
            55 => A::ResetOverline,
            58 => self.extended(slice, colon, A::UnderlineColor, A::UnderlineColor256),
            59 => A::ResetUnderlineColor,
            n @ 90..=97 => A::BrightFg8((n - 82) as u8),
            n @ 100..=107 => A::BrightBg8((n - 92) as u8),
            _ => self.unknown(slice),
        })
    }
}

/// Apply one attribute to a style (Screen.zig:2048 `setAttribute`).
pub fn apply(attr: &Attribute<'_>, style: &mut Style) {
    use Attribute as A;
    let f = &mut style.flags;
    match *attr {
        A::Unknown { .. } => {}
        A::Unset => *style = Style::default(),
        A::Bold => f.bold = true,
        A::ResetBold => {
            f.bold = false;
            f.faint = false;
        }
        A::Italic => f.italic = true,
        A::ResetItalic => f.italic = false,
        A::Faint => f.faint = true,
        A::Underline(u) => f.underline = u,
        A::UnderlineColor(c) => style.underline_color = Color::Rgb(c),
        A::UnderlineColor256(i) => style.underline_color = Color::Palette(i),
        A::ResetUnderlineColor => style.underline_color = Color::None,
        A::Overline => f.overline = true,
        A::ResetOverline => f.overline = false,
        A::Blink => f.blink = true,
        A::ResetBlink => f.blink = false,
        A::Inverse => f.inverse = true,
        A::ResetInverse => f.inverse = false,
        A::Invisible => f.invisible = true,
        A::ResetInvisible => f.invisible = false,
        A::Strikethrough => f.strikethrough = true,
        A::ResetStrikethrough => f.strikethrough = false,
        A::DirectColorFg(c) => style.fg = Color::Rgb(c),
        A::DirectColorBg(c) => style.bg = Color::Rgb(c),
        A::Fg8(i) | A::BrightFg8(i) | A::Fg256(i) => style.fg = Color::Palette(i),
        A::Bg8(i) | A::BrightBg8(i) | A::Bg256(i) => style.bg = Color::Palette(i),
        A::ResetFg => style.fg = Color::None,
        A::ResetBg => style.bg = Color::None,
    }
}

/// Apply a whole SGR sequence.
pub fn apply_all(csi: &Csi<'_>, style: &mut Style) {
    for attr in Parser::new(csi) {
        apply(&attr, style);
    }
}

#[cfg(test)]
mod tests {
    use super::Attribute as A;
    use super::*;
    use std::vec::Vec;

    /// Bits 0..n.
    fn colons(bits: &[usize]) -> u32 {
        bits.iter().fold(0, |m, &i| m | 1 << i)
    }

    /// sgr.zig:525 `testParse`.
    fn parse1(params: &[u16]) -> Attribute<'_> {
        Parser::from_parts(params, 0).next().unwrap()
    }

    /// sgr.zig:530 `testParseColon`: a colon after every parameter but the last.
    fn parse1_colon(params: &[u16]) -> Attribute<'_> {
        let bits: Vec<usize> = (0..params.len() - 1).collect();
        Parser::from_parts(params, colons(&bits)).next().unwrap()
    }

    fn all(params: &[u16], bits: &[usize]) -> Vec<Attribute<'static>> {
        Parser::from_parts(params, colons(bits)).map(erase).collect()
    }

    /// The attribute with an Unknown's borrows erased, to compare against [`UNK`].
    fn erase(a: Attribute<'_>) -> Attribute<'static> {
        match a {
            A::Unknown { .. } => A::Unknown { full: &[], partial: &[] },
            A::Unset => A::Unset,
            A::Bold => A::Bold,
            A::ResetBold => A::ResetBold,
            A::Italic => A::Italic,
            A::ResetItalic => A::ResetItalic,
            A::Faint => A::Faint,
            A::Underline(u) => A::Underline(u),
            A::UnderlineColor(c) => A::UnderlineColor(c),
            A::UnderlineColor256(i) => A::UnderlineColor256(i),
            A::ResetUnderlineColor => A::ResetUnderlineColor,
            A::Overline => A::Overline,
            A::ResetOverline => A::ResetOverline,
            A::Blink => A::Blink,
            A::ResetBlink => A::ResetBlink,
            A::Inverse => A::Inverse,
            A::ResetInverse => A::ResetInverse,
            A::Invisible => A::Invisible,
            A::ResetInvisible => A::ResetInvisible,
            A::Strikethrough => A::Strikethrough,
            A::ResetStrikethrough => A::ResetStrikethrough,
            A::DirectColorFg(c) => A::DirectColorFg(c),
            A::DirectColorBg(c) => A::DirectColorBg(c),
            A::Fg8(i) => A::Fg8(i),
            A::Bg8(i) => A::Bg8(i),
            A::BrightFg8(i) => A::BrightFg8(i),
            A::BrightBg8(i) => A::BrightBg8(i),
            A::Fg256(i) => A::Fg256(i),
            A::Bg256(i) => A::Bg256(i),
            A::ResetFg => A::ResetFg,
            A::ResetBg => A::ResetBg,
        }
    }

    const UNK: Attribute<'static> = A::Unknown { full: &[], partial: &[] };

    const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    fn is_unknown(a: &Attribute<'_>) -> bool {
        matches!(a, A::Unknown { .. })
    }

    // sgr.zig:541 "sgr: Parser"
    #[test]
    fn sgr_parser() {
        assert_eq!(parse1(&[]), A::Unset);
        assert_eq!(parse1(&[0]), A::Unset);
        assert_eq!(parse1(&[38, 2, 40, 44, 52]), A::DirectColorFg(rgb(40, 44, 52)));
        assert!(is_unknown(&parse1(&[38, 2, 44, 52])));
        assert_eq!(parse1(&[48, 2, 40, 44, 52]), A::DirectColorBg(rgb(40, 44, 52)));
        assert!(is_unknown(&parse1(&[48, 2, 44, 52])));
    }

    // sgr.zig:566 "sgr: Parser multiple"
    #[test]
    fn sgr_parser_multiple() {
        let mut p = Parser::from_parts(&[0, 38, 2, 40, 44, 52], 0);
        assert_eq!(p.next(), Some(A::Unset));
        assert_eq!(p.next(), Some(A::DirectColorFg(rgb(40, 44, 52))));
        assert_eq!(p.next(), None);
        assert_eq!(p.next(), None);
    }

    // sgr.zig:574 "sgr: unsupported with colon"
    #[test]
    fn sgr_unsupported_with_colon() {
        let params = [0, 4, 1];
        let mut p = Parser::from_parts(&params, colons(&[0]));
        assert_eq!(p.next(), Some(A::Unknown { full: &params, partial: &params[..2] }));
        assert_eq!(p.next(), Some(A::Bold));
        assert_eq!(p.next(), None);
    }

    // sgr.zig:588 "sgr: unsupported with multiple colon"
    #[test]
    fn sgr_unsupported_with_multiple_colon() {
        assert_eq!(all(&[0, 4, 2, 1], &[0, 1]), [UNK, A::Bold]);
    }

    // sgr.zig:603 "sgr: bold"
    #[test]
    fn sgr_bold() {
        assert_eq!(parse1(&[1]), A::Bold);
        assert_eq!(parse1(&[22]), A::ResetBold);
    }

    // sgr.zig:615 "sgr: italic"
    #[test]
    fn sgr_italic() {
        assert_eq!(parse1(&[3]), A::Italic);
        assert_eq!(parse1(&[23]), A::ResetItalic);
    }

    // sgr.zig:627 "sgr: underline"
    #[test]
    fn sgr_underline() {
        assert_eq!(parse1(&[4]), A::Underline(Underline::Single));
        assert_eq!(parse1(&[24]), A::Underline(Underline::None));
    }

    // sgr.zig:640 "sgr: underline styles"
    #[test]
    fn sgr_underline_styles() {
        for (n, u) in [
            (2, Underline::Double),
            (0, Underline::None),
            (1, Underline::Single),
            (3, Underline::Curly),
            (4, Underline::Dotted),
            (5, Underline::Dashed),
            // Beyond ghostty's test: an unknown style draws single.
            (9, Underline::Single),
        ] {
            assert_eq!(parse1_colon(&[4, n]), A::Underline(u));
        }
    }

    // sgr.zig:678 "sgr: underline style with more"
    #[test]
    fn sgr_underline_style_with_more() {
        assert_eq!(all(&[4, 2, 1], &[0]), [A::Underline(Underline::Double), A::Bold]);
    }

    // sgr.zig:693 "sgr: underline style with too many colons"
    #[test]
    fn sgr_underline_style_with_too_many_colons() {
        assert_eq!(all(&[4, 2, 3, 1], &[0, 1]), [UNK, A::Bold]);
    }

    // sgr.zig:709 "sgr: blink"
    #[test]
    fn sgr_blink() {
        assert_eq!(parse1(&[5]), A::Blink);
        assert_eq!(parse1(&[6]), A::Blink);
        assert_eq!(parse1(&[25]), A::ResetBlink);
    }

    // sgr.zig:726 "sgr: inverse"
    #[test]
    fn sgr_inverse() {
        assert_eq!(parse1(&[7]), A::Inverse);
        assert_eq!(parse1(&[27]), A::ResetInverse);
    }

    // sgr.zig:738 "sgr: strikethrough"
    #[test]
    fn sgr_strikethrough() {
        assert_eq!(parse1(&[9]), A::Strikethrough);
        assert_eq!(parse1(&[29]), A::ResetStrikethrough);
    }

    // sgr.zig:750 "sgr: 8 color"
    #[test]
    fn sgr_8_color() {
        // red, yellow, bright_black, bright_yellow
        assert_eq!(
            all(&[31, 43, 90, 103], &[]),
            [A::Fg8(1), A::Bg8(3), A::BrightFg8(8), A::BrightBg8(11)]
        );
    }

    // sgr.zig:778 "sgr: 256 color"
    #[test]
    fn sgr_256_color() {
        assert_eq!(all(&[38, 5, 161, 48, 5, 236], &[]), [A::Fg256(161), A::Bg256(236)]);
    }

    // sgr.zig:785 "sgr: 256 color underline"
    #[test]
    fn sgr_256_color_underline() {
        assert_eq!(all(&[58, 5, 9], &[]), [A::UnderlineColor256(9)]);
    }

    // sgr.zig:791 "sgr: 24-bit bg color"
    #[test]
    fn sgr_24_bit_bg_color() {
        assert_eq!(parse1_colon(&[48, 2, 1, 2, 3]), A::DirectColorBg(rgb(1, 2, 3)));
    }

    // sgr.zig:801 "sgr: underline color"
    #[test]
    fn sgr_underline_color() {
        assert_eq!(parse1_colon(&[58, 2, 1, 2, 3]), A::UnderlineColor(rgb(1, 2, 3)));
        assert_eq!(parse1_colon(&[58, 2, 0, 1, 2, 3]), A::UnderlineColor(rgb(1, 2, 3)));
    }

    // sgr.zig:819 "sgr: reset underline color"
    #[test]
    fn sgr_reset_underline_color() {
        assert_eq!(parse1(&[59]), A::ResetUnderlineColor);
    }

    // sgr.zig:824 "sgr: invisible"
    #[test]
    fn sgr_invisible() {
        assert_eq!(all(&[8, 28], &[]), [A::Invisible, A::ResetInvisible]);
    }

    // sgr.zig:830 "sgr: underline, bg, and fg"
    #[test]
    fn sgr_underline_bg_and_fg() {
        assert_eq!(
            all(&[4, 38, 2, 255, 247, 219, 48, 2, 242, 93, 147, 4], &[]),
            [
                A::Underline(Underline::Single),
                A::DirectColorFg(rgb(255, 247, 219)),
                A::DirectColorBg(rgb(242, 93, 147)),
                A::Underline(Underline::Single),
            ]
        );
    }

    // sgr.zig:860 "sgr: direct color fg missing color"
    #[test]
    fn sgr_direct_color_fg_missing_color() {
        // 38 without its index is unknown; the 5 after it is then read as blink.
        assert_eq!(all(&[38, 5], &[]), [UNK, A::Blink]);
    }

    // sgr.zig:866 "sgr: direct color bg missing color"
    #[test]
    fn sgr_direct_color_bg_missing_color() {
        assert_eq!(all(&[48, 5], &[]), [UNK, A::Blink]);
    }

    // sgr.zig:872 "sgr: direct fg/bg/underline ignore optional color space"
    #[test]
    fn sgr_direct_ignore_optional_color_space() {
        assert_eq!(parse1_colon(&[38, 2, 0, 1, 2, 3]), A::DirectColorFg(rgb(1, 2, 3)));
        assert_eq!(parse1_colon(&[48, 2, 0, 1, 2, 3]), A::DirectColorBg(rgb(1, 2, 3)));
        assert_eq!(parse1_colon(&[58, 2, 0, 1, 2, 3]), A::UnderlineColor(rgb(1, 2, 3)));
        // The semicolon form has no colorspace.
        assert_eq!(parse1(&[38, 2, 0, 1, 2, 3]), A::DirectColorFg(rgb(0, 1, 2)));
        assert_eq!(parse1(&[48, 2, 0, 1, 2, 3]), A::DirectColorBg(rgb(0, 1, 2)));
        assert_eq!(parse1(&[58, 2, 0, 1, 2, 3]), A::UnderlineColor(rgb(0, 1, 2)));
    }

    // sgr.zig:928 "sgr: direct fg colon with too many colons"
    #[test]
    fn sgr_direct_fg_colon_with_too_many_colons() {
        assert_eq!(all(&[38, 2, 0, 1, 2, 3, 4, 1], &[0, 1, 2, 3, 4, 5]), [UNK, A::Bold]);
    }

    // sgr.zig:943 "sgr: direct fg colon with colorspace and extra param"
    #[test]
    fn sgr_direct_fg_colon_with_colorspace_and_extra_param() {
        assert_eq!(
            all(&[38, 2, 0, 1, 2, 3, 1], &[0, 1, 2, 3, 4]),
            [A::DirectColorFg(rgb(1, 2, 3)), A::Bold]
        );
    }

    // sgr.zig:965 "sgr: direct fg colon no colorspace and extra param"
    #[test]
    fn sgr_direct_fg_colon_no_colorspace_and_extra_param() {
        assert_eq!(
            all(&[38, 2, 1, 2, 3, 1], &[0, 1, 2, 3]),
            [A::DirectColorFg(rgb(1, 2, 3)), A::Bold]
        );
    }

    // sgr.zig:988 "sgr: kakoune input"
    #[test]
    fn sgr_kakoune_input() {
        let got =
            all(&[0, 4, 3, 38, 2, 175, 175, 215, 58, 2, 0, 190, 80, 70], &[1, 8, 9, 10, 11, 12]);
        assert_eq!(
            got[..4],
            [
                A::Unset,
                A::Underline(Underline::Curly),
                A::DirectColorFg(rgb(175, 175, 215)),
                A::UnderlineColor(rgb(190, 80, 70)),
            ]
        );
    }

    // sgr.zig:1032 "sgr: kakoune input issue underline, fg, and bg"
    #[test]
    fn sgr_kakoune_input_issue_underline_fg_and_bg() {
        assert_eq!(
            all(&[4, 3, 38, 2, 51, 51, 51, 48, 2, 170, 170, 170, 58, 2, 255, 97, 136], &[0]),
            [
                A::Underline(Underline::Curly),
                A::DirectColorFg(rgb(51, 51, 51)),
                A::DirectColorBg(rgb(170, 170, 170)),
                A::UnderlineColor(rgb(255, 97, 136)),
            ]
        );
    }

    // sgr.zig:1083 "sgr: underline colon with trailing separator and short slice"
    #[test]
    fn sgr_underline_colon_with_trailing_separator_and_short_slice() {
        assert_eq!(all(&[58, 4], &[0, 1]), [UNK, UNK]);
    }

    fn styled(params: &[u16], bits: &[usize]) -> Style {
        let mut s = Style::default();
        for a in Parser::from_parts(params, colons(bits)) {
            apply(&a, &mut s);
        }
        s
    }

    // vt-inventory §6, one assertion a row: each code's effect on a style.
    #[test]
    fn inventory_rows() {
        use crate::style::Flags;
        let fl = |f: Flags| Style { flags: f, ..Style::default() };
        let on = Flags::default();
        let dirty = Style {
            fg: Color::Palette(1),
            bg: Color::Palette(2),
            underline_color: Color::Palette(3),
            flags: Flags {
                bold: true,
                italic: true,
                faint: true,
                blink: true,
                inverse: true,
                invisible: true,
                strikethrough: true,
                overline: true,
                underline: Underline::Curly,
            },
        };
        let from = |s: Style, params: &[u16], bits: &[usize]| {
            let mut s = s;
            for a in Parser::from_parts(params, colons(bits)) {
                apply(&a, &mut s);
            }
            s
        };
        // 0 / empty
        assert_eq!(from(dirty, &[0], &[]), Style::default());
        assert_eq!(from(dirty, &[], &[]), Style::default());
        assert_eq!(styled(&[1], &[]), fl(Flags { bold: true, ..on }));
        assert_eq!(styled(&[2], &[]), fl(Flags { faint: true, ..on }));
        assert_eq!(styled(&[3], &[]), fl(Flags { italic: true, ..on }));
        assert_eq!(styled(&[4], &[]), fl(Flags { underline: Underline::Single, ..on }));
        for (n, u) in [
            (0, Underline::None),
            (1, Underline::Single),
            (2, Underline::Double),
            (3, Underline::Curly),
            (4, Underline::Dotted),
            (5, Underline::Dashed),
            (6, Underline::Single),
        ] {
            assert_eq!(from(dirty, &[4, n], &[0]).flags.underline, u);
        }
        assert_eq!(styled(&[5], &[]), fl(Flags { blink: true, ..on }));
        assert_eq!(styled(&[6], &[]), fl(Flags { blink: true, ..on }));
        assert_eq!(styled(&[7], &[]), fl(Flags { inverse: true, ..on }));
        assert_eq!(styled(&[8], &[]), fl(Flags { invisible: true, ..on }));
        assert_eq!(styled(&[9], &[]), fl(Flags { strikethrough: true, ..on }));
        assert_eq!(styled(&[21], &[]), fl(Flags { underline: Underline::Double, ..on }));
        let off = |params: &[u16]| from(dirty, params, &[]);
        let r22 = off(&[22]);
        assert!(!r22.flags.bold && !r22.flags.faint && r22.flags.italic);
        assert!(!off(&[23]).flags.italic);
        assert_eq!(off(&[24]).flags.underline, Underline::None);
        assert!(!off(&[25]).flags.blink);
        assert!(!off(&[27]).flags.inverse);
        assert!(!off(&[28]).flags.invisible);
        assert!(!off(&[29]).flags.strikethrough);
        for n in 0..8u16 {
            assert_eq!(styled(&[30 + n], &[]).fg, Color::Palette(n as u8));
            assert_eq!(styled(&[40 + n], &[]).bg, Color::Palette(n as u8));
            assert_eq!(styled(&[90 + n], &[]).fg, Color::Palette(8 + n as u8));
            assert_eq!(styled(&[100 + n], &[]).bg, Color::Palette(8 + n as u8));
        }
        // 38;5;n / 38:5:n, n truncated to u8.
        assert_eq!(styled(&[38, 5, 200], &[]).fg, Color::Palette(200));
        assert_eq!(styled(&[38, 5, 200], &[0, 1]).fg, Color::Palette(200));
        assert_eq!(styled(&[38, 5, 257], &[]).fg, Color::Palette(1));
        // 38;2;r;g;b, 38:2:r:g:b, 38:2:cs:r:g:b; another colon count is invalid.
        let c = Color::Rgb(rgb(1, 2, 3));
        assert_eq!(styled(&[38, 2, 1, 2, 3], &[]).fg, c);
        assert_eq!(styled(&[38, 2, 1, 2, 3], &[0, 1, 2, 3]).fg, c);
        assert_eq!(styled(&[38, 2, 9, 1, 2, 3], &[0, 1, 2, 3, 4]).fg, c);
        assert_eq!(styled(&[38, 2, 1, 2], &[0, 1, 2]).fg, Color::None);
        assert_eq!(off(&[39]).fg, Color::None);
        assert_eq!(styled(&[48, 5, 17], &[]).bg, Color::Palette(17));
        assert_eq!(styled(&[48, 5, 17], &[0, 1]).bg, Color::Palette(17));
        assert_eq!(styled(&[48, 2, 1, 2, 3], &[]).bg, c);
        assert_eq!(styled(&[48, 2, 1, 2, 3], &[0, 1, 2, 3]).bg, c);
        assert_eq!(off(&[49]).bg, Color::None);
        assert_eq!(styled(&[53], &[]), fl(Flags { overline: true, ..on }));
        assert!(!off(&[55]).flags.overline);
        assert_eq!(styled(&[58, 5, 4], &[]).underline_color, Color::Palette(4));
        assert_eq!(styled(&[58, 5, 4], &[0, 1]).underline_color, Color::Palette(4));
        assert_eq!(styled(&[58, 2, 1, 2, 3], &[]).underline_color, c);
        assert_eq!(styled(&[58, 2, 1, 2, 3], &[0, 1, 2, 3]).underline_color, c);
        assert_eq!(off(&[59]).underline_color, Color::None);
        // Unknown codes change nothing.
        for n in [26, 50, 51, 52, 54, 56, 57, 60, 61, 62, 63, 64, 65, 73, 74, 75] {
            assert_eq!(off(&[n]), dirty, "{n}");
            assert!(is_unknown(&parse1(&[n])));
        }
        // 38:3 and 38:4 (CMY, CMYK) are unsupported: unknown, nothing applied.
        assert_eq!(from(dirty, &[38, 3, 0, 1, 2, 3], &[0, 1, 2, 3, 4]), dirty);
        assert_eq!(from(dirty, &[38, 4, 0, 1, 2, 3, 4], &[0, 1, 2, 3, 4, 5]), dirty);
        // Missing sub-parameters: unknown.
        assert_eq!(from(dirty, &[38], &[]), dirty);
        assert_eq!(from(dirty, &[58, 2, 1], &[]), dirty);
        // More than 16 parameters all apply (ghostty stream "SGR 17+ params").
        let many = [1, 2, 3, 4, 5, 7, 8, 9, 53, 31, 41, 0, 1, 3, 4, 7, 9, 53, 32];
        let s = styled(&many, &[]);
        assert!(
            s.flags.bold
                && s.flags.italic
                && s.flags.inverse
                && s.flags.strikethrough
                && s.flags.overline
        );
        assert!(!s.flags.faint && !s.flags.blink);
        assert_eq!(s.fg, Color::Palette(2));
        assert_eq!(s.bg, Color::None);
    }

    #[test]
    fn apply_all_from_csi() {
        let params = [1, 38, 5, 9];
        let csi = Csi { intermediates: &[], params: &params, colons: 0, final_byte: b'm' };
        let mut s = Style::default();
        apply_all(&csi, &mut s);
        assert!(s.flags.bold);
        assert_eq!(s.fg, Color::Palette(9));
    }

    // Property: on any parameters and colon mask the iterator ends, never yields more items than
    // parameters (plus one for an empty list), and each Unknown's partial is a suffix-prefix of
    // the parameters.
    #[test]
    fn parser_terminates() {
        blaze_sim::check(blaze_sim::Config::cases(2000), |tape| {
            let n = tape.int_in(0..=24, "len") as usize;
            let mut params = Vec::new();
            for _ in 0..n {
                let v = if tape.coin("small") {
                    *tape.pick(&[0u16, 2, 4, 5, 38, 48, 58, 1, 3, 21, 255, 256], "code").unwrap()
                } else {
                    tape.int_in(0..=65535, "v") as u16
                };
                params.push(v);
            }
            let mask = tape.int_in(0..=0xFF_FFFF, "colons") as u32;
            let mut count = 0;
            for a in Parser::from_parts(&params, mask) {
                count += 1;
                assert!(count <= params.len().max(1));
                if let A::Unknown { full, partial } = a {
                    assert_eq!(full.len(), params.len());
                    assert!(partial.len() <= params.len());
                }
                let mut s = Style::default();
                apply(&a, &mut s);
            }
        });
    }
}
