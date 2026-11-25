//! Colours as ghostty's color.zig and x11_color.zig hold them: the colour spec parser every
//! colour OSC uses (`rgb:`, `rgbi:`, `#…`, bare hex, X11 names), the default 256-colour palette,
//! a palette that remembers which entries a program changed (OSC 104 with no argument resets
//! only those), a dynamic colour with a default, xterm's special and dynamic colour numbers, and
//! the `rgb:` forms a query reply writes.
//!
//! Not ported: `generate256Color` and its CIELAB helpers (a theme-config feature of the app that
//! derives a palette from base16 colours; no escape sequence reaches it), the contrast and
//! luminance helpers (renderer), and `parsePaletteEntry` (config `N=COLOR` syntax).

pub use crate::style::Rgb;

/// A colour spec ghostty's `RGB.parse` refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidFormat;

/// Parse a colour spec (color.zig:660 `RGB.parse`). Leading and trailing spaces and tabs are
/// ignored. Accepted, in this order:
///
/// 1. `#rgb`, `#rrggbb`, `#rrrgggbbb`, `#rrrrggggbbbb` (4, 8, 12, 16 bits a channel);
/// 2. an X11 colour name, ASCII case-insensitive (spaces inside the name count);
/// 3. bare `rgb` or `rrggbb` hex (ghostty accepts these for its config);
/// 4. `rgb:r/g/b` with 1–4 hex digits a channel, each scaled from its width;
/// 5. `rgbi:r/g/b` with each a float in 0.0–1.0.
///
/// Hex digits go through Zig's `parseUnsigned`, so an underscore between digits is accepted as
/// ghostty accepts it (`rgb:f_f/0/0`). `rgbi:` floats go through core's float parser, which
/// differs from Zig's only on hex floats and underscores; NaN is refused (ghostty would trap).
pub fn parse(value: &[u8]) -> Result<Rgb, InvalidFormat> {
    let input = trim(value, b" \t");
    if input.is_empty() {
        return Err(InvalidFormat);
    }

    if input[0] == b'#' {
        let channels = |w: usize| -> Result<Rgb, InvalidFormat> {
            Ok(Rgb {
                r: from_hex(&input[1..1 + w])?,
                g: from_hex(&input[1 + w..1 + 2 * w])?,
                b: from_hex(&input[1 + 2 * w..1 + 3 * w])?,
            })
        };
        return match input.len() {
            4 => channels(1),
            7 => channels(2),
            10 => channels(3),
            13 => channels(4),
            _ => Err(InvalidFormat),
        };
    }

    if let Some(rgb) = x11(input) {
        return Ok(rgb);
    }

    match input.len() {
        3 => {
            return Ok(Rgb {
                r: from_hex(&input[0..1])?,
                g: from_hex(&input[1..2])?,
                b: from_hex(&input[2..3])?,
            });
        }
        6 => {
            return Ok(Rgb {
                r: from_hex(&input[0..2])?,
                g: from_hex(&input[2..4])?,
                b: from_hex(&input[4..6])?,
            });
        }
        _ => {}
    }

    if input.len() < b"rgb:a/a/a".len() || &input[0..3] != b"rgb" {
        return Err(InvalidFormat);
    }
    let mut i = 3;
    let intensity = input[i] == b'i';
    if intensity {
        i += 1;
    }
    if input[i] != b':' {
        return Err(InvalidFormat);
    }
    i += 1;
    let channel = |s: &[u8]| if intensity { from_intensity(s) } else { from_hex(s) };

    let end = position(input, i, b'/').ok_or(InvalidFormat)?;
    let r = channel(&input[i..end])?;
    i = end + 1;
    let end = position(input, i, b'/').ok_or(InvalidFormat)?;
    let g = channel(&input[i..end])?;
    i = end + 1;
    let b = channel(&input[i..])?;
    Ok(Rgb { r, g, b })
}

/// 1–4 hex digits, scaled from 4, 8, 12 or 16 bits to 8 (color.zig:613 `fromHex`).
fn from_hex(value: &[u8]) -> Result<u8, InvalidFormat> {
    if value.is_empty() || value.len() > 4 {
        return Err(InvalidFormat);
    }
    let color = zig_parse_unsigned(value, 16, u64::from(u16::MAX)).ok_or(InvalidFormat)?;
    let divisor: u64 = match value.len() {
        1 => 0xF,
        2 => 0xFF,
        3 => 0xFFF,
        _ => 0xFFFF,
    };
    // At most 0xFFFF * 255 / 1, so it fits; the quotient is at most 255.
    u8::try_from(color * 255 / divisor).map_err(|_| InvalidFormat)
}

/// A float in 0.0–1.0, scaled to 0–255 and truncated (color.zig:596 `fromIntensity`).
fn from_intensity(value: &[u8]) -> Result<u8, InvalidFormat> {
    let s = core::str::from_utf8(value).map_err(|_| InvalidFormat)?;
    let i: f64 = s.parse().map_err(|_| InvalidFormat)?;
    if i.is_nan() || !(0.0..=1.0).contains(&i) {
        return Err(InvalidFormat);
    }
    // In 0.0..=255.0 after the check, so the cast truncates as Zig's @intFromFloat does.
    Ok((i * 255.0) as u8)
}

fn position(s: &[u8], from: usize, b: u8) -> Option<usize> {
    s.get(from..)?.iter().position(|&c| c == b).map(|p| p + from)
}

/// `std.mem.trim`: strip any of `set` from both ends.
pub(crate) fn trim<'a>(s: &'a [u8], set: &[u8]) -> &'a [u8] {
    let start = s.iter().position(|c| !set.contains(c)).unwrap_or(s.len());
    let end = s.iter().rposition(|c| !set.contains(c)).map_or(start, |e| e + 1);
    &s[start..end]
}

/// Zig's `std.fmt.parseUnsigned(T, buf, base)` with `T`'s maximum as `max`: no sign, `_`
/// between digits ignored (not first or last), `None` for an empty string, a bad digit or
/// overflow.
pub(crate) fn zig_parse_unsigned(buf: &[u8], base: u32, max: u64) -> Option<u64> {
    if buf.is_empty() || buf[0] == b'_' || buf[buf.len() - 1] == b'_' {
        return None;
    }
    let mut acc: u64 = 0;
    for &c in buf {
        if c == b'_' {
            continue;
        }
        let d = char::from(c).to_digit(base)?;
        acc = acc.checked_mul(u64::from(base))?.checked_add(u64::from(d))?;
        if acc > max {
            return None;
        }
    }
    Some(acc)
}

/// Zig's `std.fmt.parseInt(T, buf, 10)` for `T` in `min..=max`: an optional `+` or `-`, then as
/// [`zig_parse_unsigned`]. For an unsigned `T` (`min == 0`), `-0` is 0 and any other negative
/// is an overflow.
pub(crate) fn zig_parse_int(buf: &[u8], min: i64, max: i64) -> Option<i64> {
    let (neg, digits) = match buf.first() {
        Some(b'+') => (false, &buf[1..]),
        Some(b'-') => (true, &buf[1..]),
        _ => (false, buf),
    };
    let bound = if neg { min.unsigned_abs() } else { max.unsigned_abs() };
    let mag = zig_parse_unsigned(digits, 10, bound)?;
    if neg {
        // mag <= |min|, so the negation fits in i128 and then in i64.
        i64::try_from(-i128::from(mag)).ok()
    } else {
        i64::try_from(mag).ok()
    }
}

/// An X11 colour by name (x11_color.zig `map`): ASCII case-insensitive, spaces significant
/// (`"medium spring green"` and `"MediumSpringGreen"` are both names; `"Forest Green"` is one
/// because rgb.txt lists `forest green`).
pub fn x11(name: &[u8]) -> Option<Rgb> {
    X11.binary_search_by(|(n, ..)| cmp_lower(n, name)).ok().map(|i| {
        let (_, r, g, b) = X11[i];
        Rgb { r, g, b }
    })
}

/// `table` (already lower case) against `key` lowered.
fn cmp_lower(table: &[u8], key: &[u8]) -> core::cmp::Ordering {
    table.iter().copied().cmp(key.iter().map(u8::to_ascii_lowercase))
}

/// The 16 named colours' defaults (color.zig:387 `Name.default`).
pub const BASE16: [Rgb; 16] = [
    rgb(0x1D, 0x1F, 0x21),
    rgb(0xCC, 0x66, 0x66),
    rgb(0xB5, 0xBD, 0x68),
    rgb(0xF0, 0xC6, 0x74),
    rgb(0x81, 0xA2, 0xBE),
    rgb(0xB2, 0x94, 0xBB),
    rgb(0x8A, 0xBE, 0xB7),
    rgb(0xC5, 0xC8, 0xC6),
    rgb(0x66, 0x66, 0x66),
    rgb(0xD5, 0x4E, 0x53),
    rgb(0xB9, 0xCA, 0x4A),
    rgb(0xE7, 0xC5, 0x47),
    rgb(0x7A, 0xA6, 0xDA),
    rgb(0xC3, 0x97, 0xD8),
    rgb(0x70, 0xC0, 0xB1),
    rgb(0xEA, 0xEA, 0xEA),
];

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// The 256-colour palette.
pub type Palette = [Rgb; 256];

/// The default palette (color.zig:8): the 16 named colours, the 6×6×6 cube (0, then 95 + 40k),
/// and the 24-step grey ramp from 8 by 10.
pub const DEFAULT_PALETTE: Palette = default_palette();

const fn default_palette() -> Palette {
    let mut p = [Rgb { r: 0, g: 0, b: 0 }; 256];
    let mut i = 0;
    while i < 16 {
        p[i] = BASE16[i];
        i += 1;
    }
    const fn level(v: u8) -> u8 {
        if v == 0 { 0 } else { v * 40 + 55 }
    }
    let mut r = 0;
    while r < 6 {
        let mut g = 0;
        while g < 6 {
            let mut b = 0;
            while b < 6 {
                p[i] = rgb(level(r), level(g), level(b));
                i += 1;
                b += 1;
            }
            g += 1;
        }
        r += 1;
    }
    let mut k: u8 = 0;
    while k < 24 {
        let v = k * 10 + 8;
        p[i] = rgb(v, v, v);
        i += 1;
        k += 1;
    }
    p
}

/// Which palette entries a program set (color.zig:143 `PaletteMask`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct PaletteMask([u64; 4]);

impl PaletteMask {
    pub fn set(&mut self, i: u8) {
        self.0[usize::from(i >> 6)] |= 1 << (i & 63);
    }
    pub fn unset(&mut self, i: u8) {
        self.0[usize::from(i >> 6)] &= !(1 << (i & 63));
    }
    pub fn is_set(&self, i: u8) -> bool {
        self.0[usize::from(i >> 6)] & (1 << (i & 63)) != 0
    }
    pub fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }
    /// The set indices, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        (0..=255u8).filter(|&i| self.is_set(i))
    }
}

/// A palette a program can change and reset (color.zig:270 `DynamicPalette`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DynamicPalette {
    /// What is drawn: the original with the program's changes.
    pub current: Palette,
    /// The configured palette, what a reset returns to.
    pub original: Palette,
    /// The entries a program changed since their last reset.
    pub mask: PaletteMask,
}

impl Default for DynamicPalette {
    fn default() -> Self {
        Self::new(DEFAULT_PALETTE)
    }
}

impl DynamicPalette {
    pub fn new(def: Palette) -> Self {
        Self { current: def, original: def, mask: PaletteMask::default() }
    }

    pub fn set(&mut self, i: u8, color: Rgb) {
        self.current[usize::from(i)] = color;
        self.mask.set(i);
    }

    pub fn reset(&mut self, i: u8) {
        self.current[usize::from(i)] = self.original[usize::from(i)];
        self.mask.unset(i);
    }

    pub fn reset_all(&mut self) {
        *self = Self::new(self.original);
    }

    /// OSC 104 with no index, as the libghostty handler runs it (stream_terminal.zig:716):
    /// reset only the entries the mask names. Returns whether any was.
    pub fn reset_modified(&mut self) -> bool {
        let any = self.mask.count() > 0;
        for i in 0..=255u8 {
            if self.mask.is_set(i) {
                self.reset(i);
            }
        }
        any
    }

    /// A new configured palette that keeps the program's changes (color.zig:310).
    pub fn change_default(&mut self, def: Palette) {
        self.original = def;
        let mut current = def;
        for i in self.mask.iter() {
            current[usize::from(i)] = self.current[usize::from(i)];
        }
        self.current = current;
    }
}

/// A colour that can be set and reset, maybe with no default (color.zig:331 `DynamicRGB`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DynamicRgb {
    pub override_: Option<Rgb>,
    pub default: Option<Rgb>,
}

impl DynamicRgb {
    pub const UNSET: DynamicRgb = DynamicRgb { override_: None, default: None };

    pub fn new(def: Rgb) -> Self {
        Self { override_: None, default: Some(def) }
    }
    pub fn get(&self) -> Option<Rgb> {
        self.override_.or(self.default)
    }
    pub fn set(&mut self, c: Rgb) {
        self.override_ = Some(c);
    }
    /// As ghostty: the override becomes the default (so `get` is the default).
    pub fn reset(&mut self) {
        self.override_ = self.default;
    }
}

/// The colours a terminal keeps and the colour OSCs change (Terminal.zig:140 `Colors`): plain
/// data, embedded in the terminal by its owner. `Colors::default()` is ghostty's
/// `Colors.default`: the default palette, the three dynamic colours unset (the host's config
/// gives them defaults with [`DynamicRgb::new`]).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Colors {
    pub background: DynamicRgb,
    pub foreground: DynamicRgb,
    pub cursor: DynamicRgb,
    pub palette: DynamicPalette,
}

/// xterm's special colours (color.zig:416), set by OSC 5 `n` or OSC 4 `256 + n`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Special {
    Bold = 0,
    Underline = 1,
    Blink = 2,
    Reverse = 3,
    Italic = 4,
}

impl Special {
    pub const ALL: [Special; 5] =
        [Special::Bold, Special::Underline, Special::Blink, Special::Reverse, Special::Italic];

    pub fn from_index(n: u64) -> Option<Special> {
        usize::try_from(n).ok().and_then(|n| Self::ALL.get(n).copied())
    }

    /// Its OSC 4 number: 256 + n.
    pub fn osc4(self) -> u16 {
        256 + self as u16
    }
}

/// xterm's dynamic colours (color.zig:447), OSC 10–19, reset by 110–119.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Dynamic {
    Foreground = 10,
    Background = 11,
    Cursor = 12,
    PointerForeground = 13,
    PointerBackground = 14,
    TektronixForeground = 15,
    TektronixBackground = 16,
    HighlightBackground = 17,
    TektronixCursor = 18,
    HighlightForeground = 19,
}

impl Dynamic {
    pub const ALL: [Dynamic; 10] = [
        Dynamic::Foreground,
        Dynamic::Background,
        Dynamic::Cursor,
        Dynamic::PointerForeground,
        Dynamic::PointerBackground,
        Dynamic::TektronixForeground,
        Dynamic::TektronixBackground,
        Dynamic::HighlightBackground,
        Dynamic::TektronixCursor,
        Dynamic::HighlightForeground,
    ];

    /// Its OSC number, 10–19.
    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Dynamic> {
        code.checked_sub(10).and_then(|i| Self::ALL.get(usize::from(i)).copied())
    }

    /// The next in the list: OSC 10 `a;b` sets 10 then 11 (color.zig:465).
    pub fn next(self) -> Option<Dynamic> {
        Self::from_code(self.code() + 1)
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex2(out: &mut [u8], v: u8) {
    out[0] = HEX[usize::from(v >> 4)];
    out[1] = HEX[usize::from(v & 0xF)];
}

/// `rgb:rr/gg/bb` (color.zig:519 `encodeRgb8`), what an OSC 21 reply carries.
pub fn rgb8(c: Rgb) -> [u8; 12] {
    let mut o = *b"rgb:00/00/00";
    hex2(&mut o[4..6], c.r);
    hex2(&mut o[7..9], c.g);
    hex2(&mut o[10..12], c.b);
    o
}

/// `rgb:rrrr/gggg/bbbb`, each channel × 257 (color.zig:526 `encodeRgb16`), what an OSC 4 and
/// OSC 10–12 reply carries.
pub fn rgb16(c: Rgb) -> [u8; 18] {
    let mut o = *b"rgb:0000/0000/0000";
    for (at, v) in [(4, c.r), (9, c.g), (14, c.b)] {
        hex2(&mut o[at..at + 2], v);
        hex2(&mut o[at + 2..at + 4], v);
    }
    o
}

/// Every name in X11's rgb.txt (as ghostty ships it, src/terminal/res/rgb.txt, 782 names),
/// lower case, sorted by byte, for [`x11`]. rgb.txt is MIT/X11 licensed.
static X11: [(&[u8], u8, u8, u8); 782] = [
    (b"alice blue", 240, 248, 255),
    (b"aliceblue", 240, 248, 255),
    (b"antique white", 250, 235, 215),
    (b"antiquewhite", 250, 235, 215),
    (b"antiquewhite1", 255, 239, 219),
    (b"antiquewhite2", 238, 223, 204),
    (b"antiquewhite3", 205, 192, 176),
    (b"antiquewhite4", 139, 131, 120),
    (b"aqua", 0, 255, 255),
    (b"aquamarine", 127, 255, 212),
    (b"aquamarine1", 127, 255, 212),
    (b"aquamarine2", 118, 238, 198),
    (b"aquamarine3", 102, 205, 170),
    (b"aquamarine4", 69, 139, 116),
    (b"azure", 240, 255, 255),
    (b"azure1", 240, 255, 255),
    (b"azure2", 224, 238, 238),
    (b"azure3", 193, 205, 205),
    (b"azure4", 131, 139, 139),
    (b"beige", 245, 245, 220),
    (b"bisque", 255, 228, 196),
    (b"bisque1", 255, 228, 196),
    (b"bisque2", 238, 213, 183),
    (b"bisque3", 205, 183, 158),
    (b"bisque4", 139, 125, 107),
    (b"black", 0, 0, 0),
    (b"blanched almond", 255, 235, 205),
    (b"blanchedalmond", 255, 235, 205),
    (b"blue", 0, 0, 255),
    (b"blue violet", 138, 43, 226),
    (b"blue1", 0, 0, 255),
    (b"blue2", 0, 0, 238),
    (b"blue3", 0, 0, 205),
    (b"blue4", 0, 0, 139),
    (b"blueviolet", 138, 43, 226),
    (b"brown", 165, 42, 42),
    (b"brown1", 255, 64, 64),
    (b"brown2", 238, 59, 59),
    (b"brown3", 205, 51, 51),
    (b"brown4", 139, 35, 35),
    (b"burlywood", 222, 184, 135),
    (b"burlywood1", 255, 211, 155),
    (b"burlywood2", 238, 197, 145),
    (b"burlywood3", 205, 170, 125),
    (b"burlywood4", 139, 115, 85),
    (b"cadet blue", 95, 158, 160),
    (b"cadetblue", 95, 158, 160),
    (b"cadetblue1", 152, 245, 255),
    (b"cadetblue2", 142, 229, 238),
    (b"cadetblue3", 122, 197, 205),
    (b"cadetblue4", 83, 134, 139),
    (b"chartreuse", 127, 255, 0),
    (b"chartreuse1", 127, 255, 0),
    (b"chartreuse2", 118, 238, 0),
    (b"chartreuse3", 102, 205, 0),
    (b"chartreuse4", 69, 139, 0),
    (b"chocolate", 210, 105, 30),
    (b"chocolate1", 255, 127, 36),
    (b"chocolate2", 238, 118, 33),
    (b"chocolate3", 205, 102, 29),
    (b"chocolate4", 139, 69, 19),
    (b"coral", 255, 127, 80),
    (b"coral1", 255, 114, 86),
    (b"coral2", 238, 106, 80),
    (b"coral3", 205, 91, 69),
    (b"coral4", 139, 62, 47),
    (b"cornflower blue", 100, 149, 237),
    (b"cornflowerblue", 100, 149, 237),
    (b"cornsilk", 255, 248, 220),
    (b"cornsilk1", 255, 248, 220),
    (b"cornsilk2", 238, 232, 205),
    (b"cornsilk3", 205, 200, 177),
    (b"cornsilk4", 139, 136, 120),
    (b"crimson", 220, 20, 60),
    (b"cyan", 0, 255, 255),
    (b"cyan1", 0, 255, 255),
    (b"cyan2", 0, 238, 238),
    (b"cyan3", 0, 205, 205),
    (b"cyan4", 0, 139, 139),
    (b"dark blue", 0, 0, 139),
    (b"dark cyan", 0, 139, 139),
    (b"dark goldenrod", 184, 134, 11),
    (b"dark gray", 169, 169, 169),
    (b"dark green", 0, 100, 0),
    (b"dark grey", 169, 169, 169),
    (b"dark khaki", 189, 183, 107),
    (b"dark magenta", 139, 0, 139),
    (b"dark olive green", 85, 107, 47),
    (b"dark orange", 255, 140, 0),
    (b"dark orchid", 153, 50, 204),
    (b"dark red", 139, 0, 0),
    (b"dark salmon", 233, 150, 122),
    (b"dark sea green", 143, 188, 143),
    (b"dark slate blue", 72, 61, 139),
    (b"dark slate gray", 47, 79, 79),
    (b"dark slate grey", 47, 79, 79),
    (b"dark turquoise", 0, 206, 209),
    (b"dark violet", 148, 0, 211),
    (b"darkblue", 0, 0, 139),
    (b"darkcyan", 0, 139, 139),
    (b"darkgoldenrod", 184, 134, 11),
    (b"darkgoldenrod1", 255, 185, 15),
    (b"darkgoldenrod2", 238, 173, 14),
    (b"darkgoldenrod3", 205, 149, 12),
    (b"darkgoldenrod4", 139, 101, 8),
    (b"darkgray", 169, 169, 169),
    (b"darkgreen", 0, 100, 0),
    (b"darkgrey", 169, 169, 169),
    (b"darkkhaki", 189, 183, 107),
    (b"darkmagenta", 139, 0, 139),
    (b"darkolivegreen", 85, 107, 47),
    (b"darkolivegreen1", 202, 255, 112),
    (b"darkolivegreen2", 188, 238, 104),
    (b"darkolivegreen3", 162, 205, 90),
    (b"darkolivegreen4", 110, 139, 61),
    (b"darkorange", 255, 140, 0),
    (b"darkorange1", 255, 127, 0),
    (b"darkorange2", 238, 118, 0),
    (b"darkorange3", 205, 102, 0),
    (b"darkorange4", 139, 69, 0),
    (b"darkorchid", 153, 50, 204),
    (b"darkorchid1", 191, 62, 255),
    (b"darkorchid2", 178, 58, 238),
    (b"darkorchid3", 154, 50, 205),
    (b"darkorchid4", 104, 34, 139),
    (b"darkred", 139, 0, 0),
    (b"darksalmon", 233, 150, 122),
    (b"darkseagreen", 143, 188, 143),
    (b"darkseagreen1", 193, 255, 193),
    (b"darkseagreen2", 180, 238, 180),
    (b"darkseagreen3", 155, 205, 155),
    (b"darkseagreen4", 105, 139, 105),
    (b"darkslateblue", 72, 61, 139),
    (b"darkslategray", 47, 79, 79),
    (b"darkslategray1", 151, 255, 255),
    (b"darkslategray2", 141, 238, 238),
    (b"darkslategray3", 121, 205, 205),
    (b"darkslategray4", 82, 139, 139),
    (b"darkslategrey", 47, 79, 79),
    (b"darkturquoise", 0, 206, 209),
    (b"darkviolet", 148, 0, 211),
    (b"deep pink", 255, 20, 147),
    (b"deep sky blue", 0, 191, 255),
    (b"deeppink", 255, 20, 147),
    (b"deeppink1", 255, 20, 147),
    (b"deeppink2", 238, 18, 137),
    (b"deeppink3", 205, 16, 118),
    (b"deeppink4", 139, 10, 80),
    (b"deepskyblue", 0, 191, 255),
    (b"deepskyblue1", 0, 191, 255),
    (b"deepskyblue2", 0, 178, 238),
    (b"deepskyblue3", 0, 154, 205),
    (b"deepskyblue4", 0, 104, 139),
    (b"dim gray", 105, 105, 105),
    (b"dim grey", 105, 105, 105),
    (b"dimgray", 105, 105, 105),
    (b"dimgrey", 105, 105, 105),
    (b"dodger blue", 30, 144, 255),
    (b"dodgerblue", 30, 144, 255),
    (b"dodgerblue1", 30, 144, 255),
    (b"dodgerblue2", 28, 134, 238),
    (b"dodgerblue3", 24, 116, 205),
    (b"dodgerblue4", 16, 78, 139),
    (b"firebrick", 178, 34, 34),
    (b"firebrick1", 255, 48, 48),
    (b"firebrick2", 238, 44, 44),
    (b"firebrick3", 205, 38, 38),
    (b"firebrick4", 139, 26, 26),
    (b"floral white", 255, 250, 240),
    (b"floralwhite", 255, 250, 240),
    (b"forest green", 34, 139, 34),
    (b"forestgreen", 34, 139, 34),
    (b"fuchsia", 255, 0, 255),
    (b"gainsboro", 220, 220, 220),
    (b"ghost white", 248, 248, 255),
    (b"ghostwhite", 248, 248, 255),
    (b"gold", 255, 215, 0),
    (b"gold1", 255, 215, 0),
    (b"gold2", 238, 201, 0),
    (b"gold3", 205, 173, 0),
    (b"gold4", 139, 117, 0),
    (b"goldenrod", 218, 165, 32),
    (b"goldenrod1", 255, 193, 37),
    (b"goldenrod2", 238, 180, 34),
    (b"goldenrod3", 205, 155, 29),
    (b"goldenrod4", 139, 105, 20),
    (b"gray", 190, 190, 190),
    (b"gray0", 0, 0, 0),
    (b"gray1", 3, 3, 3),
    (b"gray10", 26, 26, 26),
    (b"gray100", 255, 255, 255),
    (b"gray11", 28, 28, 28),
    (b"gray12", 31, 31, 31),
    (b"gray13", 33, 33, 33),
    (b"gray14", 36, 36, 36),
    (b"gray15", 38, 38, 38),
    (b"gray16", 41, 41, 41),
    (b"gray17", 43, 43, 43),
    (b"gray18", 46, 46, 46),
    (b"gray19", 48, 48, 48),
    (b"gray2", 5, 5, 5),
    (b"gray20", 51, 51, 51),
    (b"gray21", 54, 54, 54),
    (b"gray22", 56, 56, 56),
    (b"gray23", 59, 59, 59),
    (b"gray24", 61, 61, 61),
    (b"gray25", 64, 64, 64),
    (b"gray26", 66, 66, 66),
    (b"gray27", 69, 69, 69),
    (b"gray28", 71, 71, 71),
    (b"gray29", 74, 74, 74),
    (b"gray3", 8, 8, 8),
    (b"gray30", 77, 77, 77),
    (b"gray31", 79, 79, 79),
    (b"gray32", 82, 82, 82),
    (b"gray33", 84, 84, 84),
    (b"gray34", 87, 87, 87),
    (b"gray35", 89, 89, 89),
    (b"gray36", 92, 92, 92),
    (b"gray37", 94, 94, 94),
    (b"gray38", 97, 97, 97),
    (b"gray39", 99, 99, 99),
    (b"gray4", 10, 10, 10),
    (b"gray40", 102, 102, 102),
    (b"gray41", 105, 105, 105),
    (b"gray42", 107, 107, 107),
    (b"gray43", 110, 110, 110),
    (b"gray44", 112, 112, 112),
    (b"gray45", 115, 115, 115),
    (b"gray46", 117, 117, 117),
    (b"gray47", 120, 120, 120),
    (b"gray48", 122, 122, 122),
    (b"gray49", 125, 125, 125),
    (b"gray5", 13, 13, 13),
    (b"gray50", 127, 127, 127),
    (b"gray51", 130, 130, 130),
    (b"gray52", 133, 133, 133),
    (b"gray53", 135, 135, 135),
    (b"gray54", 138, 138, 138),
    (b"gray55", 140, 140, 140),
    (b"gray56", 143, 143, 143),
    (b"gray57", 145, 145, 145),
    (b"gray58", 148, 148, 148),
    (b"gray59", 150, 150, 150),
    (b"gray6", 15, 15, 15),
    (b"gray60", 153, 153, 153),
    (b"gray61", 156, 156, 156),
    (b"gray62", 158, 158, 158),
    (b"gray63", 161, 161, 161),
    (b"gray64", 163, 163, 163),
    (b"gray65", 166, 166, 166),
    (b"gray66", 168, 168, 168),
    (b"gray67", 171, 171, 171),
    (b"gray68", 173, 173, 173),
    (b"gray69", 176, 176, 176),
    (b"gray7", 18, 18, 18),
    (b"gray70", 179, 179, 179),
    (b"gray71", 181, 181, 181),
    (b"gray72", 184, 184, 184),
    (b"gray73", 186, 186, 186),
    (b"gray74", 189, 189, 189),
    (b"gray75", 191, 191, 191),
    (b"gray76", 194, 194, 194),
    (b"gray77", 196, 196, 196),
    (b"gray78", 199, 199, 199),
    (b"gray79", 201, 201, 201),
    (b"gray8", 20, 20, 20),
    (b"gray80", 204, 204, 204),
    (b"gray81", 207, 207, 207),
    (b"gray82", 209, 209, 209),
    (b"gray83", 212, 212, 212),
    (b"gray84", 214, 214, 214),
    (b"gray85", 217, 217, 217),
    (b"gray86", 219, 219, 219),
    (b"gray87", 222, 222, 222),
    (b"gray88", 224, 224, 224),
    (b"gray89", 227, 227, 227),
    (b"gray9", 23, 23, 23),
    (b"gray90", 229, 229, 229),
    (b"gray91", 232, 232, 232),
    (b"gray92", 235, 235, 235),
    (b"gray93", 237, 237, 237),
    (b"gray94", 240, 240, 240),
    (b"gray95", 242, 242, 242),
    (b"gray96", 245, 245, 245),
    (b"gray97", 247, 247, 247),
    (b"gray98", 250, 250, 250),
    (b"gray99", 252, 252, 252),
    (b"green", 0, 255, 0),
    (b"green yellow", 173, 255, 47),
    (b"green1", 0, 255, 0),
    (b"green2", 0, 238, 0),
    (b"green3", 0, 205, 0),
    (b"green4", 0, 139, 0),
    (b"greenyellow", 173, 255, 47),
    (b"grey", 190, 190, 190),
    (b"grey0", 0, 0, 0),
    (b"grey1", 3, 3, 3),
    (b"grey10", 26, 26, 26),
    (b"grey100", 255, 255, 255),
    (b"grey11", 28, 28, 28),
    (b"grey12", 31, 31, 31),
    (b"grey13", 33, 33, 33),
    (b"grey14", 36, 36, 36),
    (b"grey15", 38, 38, 38),
    (b"grey16", 41, 41, 41),
    (b"grey17", 43, 43, 43),
    (b"grey18", 46, 46, 46),
    (b"grey19", 48, 48, 48),
    (b"grey2", 5, 5, 5),
    (b"grey20", 51, 51, 51),
    (b"grey21", 54, 54, 54),
    (b"grey22", 56, 56, 56),
    (b"grey23", 59, 59, 59),
    (b"grey24", 61, 61, 61),
    (b"grey25", 64, 64, 64),
    (b"grey26", 66, 66, 66),
    (b"grey27", 69, 69, 69),
    (b"grey28", 71, 71, 71),
    (b"grey29", 74, 74, 74),
    (b"grey3", 8, 8, 8),
    (b"grey30", 77, 77, 77),
    (b"grey31", 79, 79, 79),
    (b"grey32", 82, 82, 82),
    (b"grey33", 84, 84, 84),
    (b"grey34", 87, 87, 87),
    (b"grey35", 89, 89, 89),
    (b"grey36", 92, 92, 92),
    (b"grey37", 94, 94, 94),
    (b"grey38", 97, 97, 97),
    (b"grey39", 99, 99, 99),
    (b"grey4", 10, 10, 10),
    (b"grey40", 102, 102, 102),
    (b"grey41", 105, 105, 105),
    (b"grey42", 107, 107, 107),
    (b"grey43", 110, 110, 110),
    (b"grey44", 112, 112, 112),
    (b"grey45", 115, 115, 115),
    (b"grey46", 117, 117, 117),
    (b"grey47", 120, 120, 120),
    (b"grey48", 122, 122, 122),
    (b"grey49", 125, 125, 125),
    (b"grey5", 13, 13, 13),
    (b"grey50", 127, 127, 127),
    (b"grey51", 130, 130, 130),
    (b"grey52", 133, 133, 133),
    (b"grey53", 135, 135, 135),
    (b"grey54", 138, 138, 138),
    (b"grey55", 140, 140, 140),
    (b"grey56", 143, 143, 143),
    (b"grey57", 145, 145, 145),
    (b"grey58", 148, 148, 148),
    (b"grey59", 150, 150, 150),
    (b"grey6", 15, 15, 15),
    (b"grey60", 153, 153, 153),
    (b"grey61", 156, 156, 156),
    (b"grey62", 158, 158, 158),
    (b"grey63", 161, 161, 161),
    (b"grey64", 163, 163, 163),
    (b"grey65", 166, 166, 166),
    (b"grey66", 168, 168, 168),
    (b"grey67", 171, 171, 171),
    (b"grey68", 173, 173, 173),
    (b"grey69", 176, 176, 176),
    (b"grey7", 18, 18, 18),
    (b"grey70", 179, 179, 179),
    (b"grey71", 181, 181, 181),
    (b"grey72", 184, 184, 184),
    (b"grey73", 186, 186, 186),
    (b"grey74", 189, 189, 189),
    (b"grey75", 191, 191, 191),
    (b"grey76", 194, 194, 194),
    (b"grey77", 196, 196, 196),
    (b"grey78", 199, 199, 199),
    (b"grey79", 201, 201, 201),
    (b"grey8", 20, 20, 20),
    (b"grey80", 204, 204, 204),
    (b"grey81", 207, 207, 207),
    (b"grey82", 209, 209, 209),
    (b"grey83", 212, 212, 212),
    (b"grey84", 214, 214, 214),
    (b"grey85", 217, 217, 217),
    (b"grey86", 219, 219, 219),
    (b"grey87", 222, 222, 222),
    (b"grey88", 224, 224, 224),
    (b"grey89", 227, 227, 227),
    (b"grey9", 23, 23, 23),
    (b"grey90", 229, 229, 229),
    (b"grey91", 232, 232, 232),
    (b"grey92", 235, 235, 235),
    (b"grey93", 237, 237, 237),
    (b"grey94", 240, 240, 240),
    (b"grey95", 242, 242, 242),
    (b"grey96", 245, 245, 245),
    (b"grey97", 247, 247, 247),
    (b"grey98", 250, 250, 250),
    (b"grey99", 252, 252, 252),
    (b"honeydew", 240, 255, 240),
    (b"honeydew1", 240, 255, 240),
    (b"honeydew2", 224, 238, 224),
    (b"honeydew3", 193, 205, 193),
    (b"honeydew4", 131, 139, 131),
    (b"hot pink", 255, 105, 180),
    (b"hotpink", 255, 105, 180),
    (b"hotpink1", 255, 110, 180),
    (b"hotpink2", 238, 106, 167),
    (b"hotpink3", 205, 96, 144),
    (b"hotpink4", 139, 58, 98),
    (b"indian red", 205, 92, 92),
    (b"indianred", 205, 92, 92),
    (b"indianred1", 255, 106, 106),
    (b"indianred2", 238, 99, 99),
    (b"indianred3", 205, 85, 85),
    (b"indianred4", 139, 58, 58),
    (b"indigo", 75, 0, 130),
    (b"ivory", 255, 255, 240),
    (b"ivory1", 255, 255, 240),
    (b"ivory2", 238, 238, 224),
    (b"ivory3", 205, 205, 193),
    (b"ivory4", 139, 139, 131),
    (b"khaki", 240, 230, 140),
    (b"khaki1", 255, 246, 143),
    (b"khaki2", 238, 230, 133),
    (b"khaki3", 205, 198, 115),
    (b"khaki4", 139, 134, 78),
    (b"lavender", 230, 230, 250),
    (b"lavender blush", 255, 240, 245),
    (b"lavenderblush", 255, 240, 245),
    (b"lavenderblush1", 255, 240, 245),
    (b"lavenderblush2", 238, 224, 229),
    (b"lavenderblush3", 205, 193, 197),
    (b"lavenderblush4", 139, 131, 134),
    (b"lawn green", 124, 252, 0),
    (b"lawngreen", 124, 252, 0),
    (b"lemon chiffon", 255, 250, 205),
    (b"lemonchiffon", 255, 250, 205),
    (b"lemonchiffon1", 255, 250, 205),
    (b"lemonchiffon2", 238, 233, 191),
    (b"lemonchiffon3", 205, 201, 165),
    (b"lemonchiffon4", 139, 137, 112),
    (b"light blue", 173, 216, 230),
    (b"light coral", 240, 128, 128),
    (b"light cyan", 224, 255, 255),
    (b"light goldenrod", 238, 221, 130),
    (b"light goldenrod yellow", 250, 250, 210),
    (b"light gray", 211, 211, 211),
    (b"light green", 144, 238, 144),
    (b"light grey", 211, 211, 211),
    (b"light pink", 255, 182, 193),
    (b"light salmon", 255, 160, 122),
    (b"light sea green", 32, 178, 170),
    (b"light sky blue", 135, 206, 250),
    (b"light slate blue", 132, 112, 255),
    (b"light slate gray", 119, 136, 153),
    (b"light slate grey", 119, 136, 153),
    (b"light steel blue", 176, 196, 222),
    (b"light yellow", 255, 255, 224),
    (b"lightblue", 173, 216, 230),
    (b"lightblue1", 191, 239, 255),
    (b"lightblue2", 178, 223, 238),
    (b"lightblue3", 154, 192, 205),
    (b"lightblue4", 104, 131, 139),
    (b"lightcoral", 240, 128, 128),
    (b"lightcyan", 224, 255, 255),
    (b"lightcyan1", 224, 255, 255),
    (b"lightcyan2", 209, 238, 238),
    (b"lightcyan3", 180, 205, 205),
    (b"lightcyan4", 122, 139, 139),
    (b"lightgoldenrod", 238, 221, 130),
    (b"lightgoldenrod1", 255, 236, 139),
    (b"lightgoldenrod2", 238, 220, 130),
    (b"lightgoldenrod3", 205, 190, 112),
    (b"lightgoldenrod4", 139, 129, 76),
    (b"lightgoldenrodyellow", 250, 250, 210),
    (b"lightgray", 211, 211, 211),
    (b"lightgreen", 144, 238, 144),
    (b"lightgrey", 211, 211, 211),
    (b"lightpink", 255, 182, 193),
    (b"lightpink1", 255, 174, 185),
    (b"lightpink2", 238, 162, 173),
    (b"lightpink3", 205, 140, 149),
    (b"lightpink4", 139, 95, 101),
    (b"lightsalmon", 255, 160, 122),
    (b"lightsalmon1", 255, 160, 122),
    (b"lightsalmon2", 238, 149, 114),
    (b"lightsalmon3", 205, 129, 98),
    (b"lightsalmon4", 139, 87, 66),
    (b"lightseagreen", 32, 178, 170),
    (b"lightskyblue", 135, 206, 250),
    (b"lightskyblue1", 176, 226, 255),
    (b"lightskyblue2", 164, 211, 238),
    (b"lightskyblue3", 141, 182, 205),
    (b"lightskyblue4", 96, 123, 139),
    (b"lightslateblue", 132, 112, 255),
    (b"lightslategray", 119, 136, 153),
    (b"lightslategrey", 119, 136, 153),
    (b"lightsteelblue", 176, 196, 222),
    (b"lightsteelblue1", 202, 225, 255),
    (b"lightsteelblue2", 188, 210, 238),
    (b"lightsteelblue3", 162, 181, 205),
    (b"lightsteelblue4", 110, 123, 139),
    (b"lightyellow", 255, 255, 224),
    (b"lightyellow1", 255, 255, 224),
    (b"lightyellow2", 238, 238, 209),
    (b"lightyellow3", 205, 205, 180),
    (b"lightyellow4", 139, 139, 122),
    (b"lime", 0, 255, 0),
    (b"lime green", 50, 205, 50),
    (b"limegreen", 50, 205, 50),
    (b"linen", 250, 240, 230),
    (b"magenta", 255, 0, 255),
    (b"magenta1", 255, 0, 255),
    (b"magenta2", 238, 0, 238),
    (b"magenta3", 205, 0, 205),
    (b"magenta4", 139, 0, 139),
    (b"maroon", 176, 48, 96),
    (b"maroon1", 255, 52, 179),
    (b"maroon2", 238, 48, 167),
    (b"maroon3", 205, 41, 144),
    (b"maroon4", 139, 28, 98),
    (b"medium aquamarine", 102, 205, 170),
    (b"medium blue", 0, 0, 205),
    (b"medium orchid", 186, 85, 211),
    (b"medium purple", 147, 112, 219),
    (b"medium sea green", 60, 179, 113),
    (b"medium slate blue", 123, 104, 238),
    (b"medium spring green", 0, 250, 154),
    (b"medium turquoise", 72, 209, 204),
    (b"medium violet red", 199, 21, 133),
    (b"mediumaquamarine", 102, 205, 170),
    (b"mediumblue", 0, 0, 205),
    (b"mediumorchid", 186, 85, 211),
    (b"mediumorchid1", 224, 102, 255),
    (b"mediumorchid2", 209, 95, 238),
    (b"mediumorchid3", 180, 82, 205),
    (b"mediumorchid4", 122, 55, 139),
    (b"mediumpurple", 147, 112, 219),
    (b"mediumpurple1", 171, 130, 255),
    (b"mediumpurple2", 159, 121, 238),
    (b"mediumpurple3", 137, 104, 205),
    (b"mediumpurple4", 93, 71, 139),
    (b"mediumseagreen", 60, 179, 113),
    (b"mediumslateblue", 123, 104, 238),
    (b"mediumspringgreen", 0, 250, 154),
    (b"mediumturquoise", 72, 209, 204),
    (b"mediumvioletred", 199, 21, 133),
    (b"midnight blue", 25, 25, 112),
    (b"midnightblue", 25, 25, 112),
    (b"mint cream", 245, 255, 250),
    (b"mintcream", 245, 255, 250),
    (b"misty rose", 255, 228, 225),
    (b"mistyrose", 255, 228, 225),
    (b"mistyrose1", 255, 228, 225),
    (b"mistyrose2", 238, 213, 210),
    (b"mistyrose3", 205, 183, 181),
    (b"mistyrose4", 139, 125, 123),
    (b"moccasin", 255, 228, 181),
    (b"navajo white", 255, 222, 173),
    (b"navajowhite", 255, 222, 173),
    (b"navajowhite1", 255, 222, 173),
    (b"navajowhite2", 238, 207, 161),
    (b"navajowhite3", 205, 179, 139),
    (b"navajowhite4", 139, 121, 94),
    (b"navy", 0, 0, 128),
    (b"navy blue", 0, 0, 128),
    (b"navyblue", 0, 0, 128),
    (b"old lace", 253, 245, 230),
    (b"oldlace", 253, 245, 230),
    (b"olive", 128, 128, 0),
    (b"olive drab", 107, 142, 35),
    (b"olivedrab", 107, 142, 35),
    (b"olivedrab1", 192, 255, 62),
    (b"olivedrab2", 179, 238, 58),
    (b"olivedrab3", 154, 205, 50),
    (b"olivedrab4", 105, 139, 34),
    (b"orange", 255, 165, 0),
    (b"orange red", 255, 69, 0),
    (b"orange1", 255, 165, 0),
    (b"orange2", 238, 154, 0),
    (b"orange3", 205, 133, 0),
    (b"orange4", 139, 90, 0),
    (b"orangered", 255, 69, 0),
    (b"orangered1", 255, 69, 0),
    (b"orangered2", 238, 64, 0),
    (b"orangered3", 205, 55, 0),
    (b"orangered4", 139, 37, 0),
    (b"orchid", 218, 112, 214),
    (b"orchid1", 255, 131, 250),
    (b"orchid2", 238, 122, 233),
    (b"orchid3", 205, 105, 201),
    (b"orchid4", 139, 71, 137),
    (b"pale goldenrod", 238, 232, 170),
    (b"pale green", 152, 251, 152),
    (b"pale turquoise", 175, 238, 238),
    (b"pale violet red", 219, 112, 147),
    (b"palegoldenrod", 238, 232, 170),
    (b"palegreen", 152, 251, 152),
    (b"palegreen1", 154, 255, 154),
    (b"palegreen2", 144, 238, 144),
    (b"palegreen3", 124, 205, 124),
    (b"palegreen4", 84, 139, 84),
    (b"paleturquoise", 175, 238, 238),
    (b"paleturquoise1", 187, 255, 255),
    (b"paleturquoise2", 174, 238, 238),
    (b"paleturquoise3", 150, 205, 205),
    (b"paleturquoise4", 102, 139, 139),
    (b"palevioletred", 219, 112, 147),
    (b"palevioletred1", 255, 130, 171),
    (b"palevioletred2", 238, 121, 159),
    (b"palevioletred3", 205, 104, 137),
    (b"palevioletred4", 139, 71, 93),
    (b"papaya whip", 255, 239, 213),
    (b"papayawhip", 255, 239, 213),
    (b"peach puff", 255, 218, 185),
    (b"peachpuff", 255, 218, 185),
    (b"peachpuff1", 255, 218, 185),
    (b"peachpuff2", 238, 203, 173),
    (b"peachpuff3", 205, 175, 149),
    (b"peachpuff4", 139, 119, 101),
    (b"peru", 205, 133, 63),
    (b"pink", 255, 192, 203),
    (b"pink1", 255, 181, 197),
    (b"pink2", 238, 169, 184),
    (b"pink3", 205, 145, 158),
    (b"pink4", 139, 99, 108),
    (b"plum", 221, 160, 221),
    (b"plum1", 255, 187, 255),
    (b"plum2", 238, 174, 238),
    (b"plum3", 205, 150, 205),
    (b"plum4", 139, 102, 139),
    (b"powder blue", 176, 224, 230),
    (b"powderblue", 176, 224, 230),
    (b"purple", 160, 32, 240),
    (b"purple1", 155, 48, 255),
    (b"purple2", 145, 44, 238),
    (b"purple3", 125, 38, 205),
    (b"purple4", 85, 26, 139),
    (b"rebecca purple", 102, 51, 153),
    (b"rebeccapurple", 102, 51, 153),
    (b"red", 255, 0, 0),
    (b"red1", 255, 0, 0),
    (b"red2", 238, 0, 0),
    (b"red3", 205, 0, 0),
    (b"red4", 139, 0, 0),
    (b"rosy brown", 188, 143, 143),
    (b"rosybrown", 188, 143, 143),
    (b"rosybrown1", 255, 193, 193),
    (b"rosybrown2", 238, 180, 180),
    (b"rosybrown3", 205, 155, 155),
    (b"rosybrown4", 139, 105, 105),
    (b"royal blue", 65, 105, 225),
    (b"royalblue", 65, 105, 225),
    (b"royalblue1", 72, 118, 255),
    (b"royalblue2", 67, 110, 238),
    (b"royalblue3", 58, 95, 205),
    (b"royalblue4", 39, 64, 139),
    (b"saddle brown", 139, 69, 19),
    (b"saddlebrown", 139, 69, 19),
    (b"salmon", 250, 128, 114),
    (b"salmon1", 255, 140, 105),
    (b"salmon2", 238, 130, 98),
    (b"salmon3", 205, 112, 84),
    (b"salmon4", 139, 76, 57),
    (b"sandy brown", 244, 164, 96),
    (b"sandybrown", 244, 164, 96),
    (b"sea green", 46, 139, 87),
    (b"seagreen", 46, 139, 87),
    (b"seagreen1", 84, 255, 159),
    (b"seagreen2", 78, 238, 148),
    (b"seagreen3", 67, 205, 128),
    (b"seagreen4", 46, 139, 87),
    (b"seashell", 255, 245, 238),
    (b"seashell1", 255, 245, 238),
    (b"seashell2", 238, 229, 222),
    (b"seashell3", 205, 197, 191),
    (b"seashell4", 139, 134, 130),
    (b"sienna", 160, 82, 45),
    (b"sienna1", 255, 130, 71),
    (b"sienna2", 238, 121, 66),
    (b"sienna3", 205, 104, 57),
    (b"sienna4", 139, 71, 38),
    (b"silver", 192, 192, 192),
    (b"sky blue", 135, 206, 235),
    (b"skyblue", 135, 206, 235),
    (b"skyblue1", 135, 206, 255),
    (b"skyblue2", 126, 192, 238),
    (b"skyblue3", 108, 166, 205),
    (b"skyblue4", 74, 112, 139),
    (b"slate blue", 106, 90, 205),
    (b"slate gray", 112, 128, 144),
    (b"slate grey", 112, 128, 144),
    (b"slateblue", 106, 90, 205),
    (b"slateblue1", 131, 111, 255),
    (b"slateblue2", 122, 103, 238),
    (b"slateblue3", 105, 89, 205),
    (b"slateblue4", 71, 60, 139),
    (b"slategray", 112, 128, 144),
    (b"slategray1", 198, 226, 255),
    (b"slategray2", 185, 211, 238),
    (b"slategray3", 159, 182, 205),
    (b"slategray4", 108, 123, 139),
    (b"slategrey", 112, 128, 144),
    (b"snow", 255, 250, 250),
    (b"snow1", 255, 250, 250),
    (b"snow2", 238, 233, 233),
    (b"snow3", 205, 201, 201),
    (b"snow4", 139, 137, 137),
    (b"spring green", 0, 255, 127),
    (b"springgreen", 0, 255, 127),
    (b"springgreen1", 0, 255, 127),
    (b"springgreen2", 0, 238, 118),
    (b"springgreen3", 0, 205, 102),
    (b"springgreen4", 0, 139, 69),
    (b"steel blue", 70, 130, 180),
    (b"steelblue", 70, 130, 180),
    (b"steelblue1", 99, 184, 255),
    (b"steelblue2", 92, 172, 238),
    (b"steelblue3", 79, 148, 205),
    (b"steelblue4", 54, 100, 139),
    (b"tan", 210, 180, 140),
    (b"tan1", 255, 165, 79),
    (b"tan2", 238, 154, 73),
    (b"tan3", 205, 133, 63),
    (b"tan4", 139, 90, 43),
    (b"teal", 0, 128, 128),
    (b"thistle", 216, 191, 216),
    (b"thistle1", 255, 225, 255),
    (b"thistle2", 238, 210, 238),
    (b"thistle3", 205, 181, 205),
    (b"thistle4", 139, 123, 139),
    (b"tomato", 255, 99, 71),
    (b"tomato1", 255, 99, 71),
    (b"tomato2", 238, 92, 66),
    (b"tomato3", 205, 79, 57),
    (b"tomato4", 139, 54, 38),
    (b"turquoise", 64, 224, 208),
    (b"turquoise1", 0, 245, 255),
    (b"turquoise2", 0, 229, 238),
    (b"turquoise3", 0, 197, 205),
    (b"turquoise4", 0, 134, 139),
    (b"violet", 238, 130, 238),
    (b"violet red", 208, 32, 144),
    (b"violetred", 208, 32, 144),
    (b"violetred1", 255, 62, 150),
    (b"violetred2", 238, 58, 140),
    (b"violetred3", 205, 50, 120),
    (b"violetred4", 139, 34, 82),
    (b"web gray", 128, 128, 128),
    (b"web green", 0, 128, 0),
    (b"web grey", 128, 128, 128),
    (b"web maroon", 128, 0, 0),
    (b"web purple", 128, 0, 128),
    (b"webgray", 128, 128, 128),
    (b"webgreen", 0, 128, 0),
    (b"webgrey", 128, 128, 128),
    (b"webmaroon", 128, 0, 0),
    (b"webpurple", 128, 0, 128),
    (b"wheat", 245, 222, 179),
    (b"wheat1", 255, 231, 186),
    (b"wheat2", 238, 216, 174),
    (b"wheat3", 205, 186, 150),
    (b"wheat4", 139, 126, 102),
    (b"white", 255, 255, 255),
    (b"white smoke", 245, 245, 245),
    (b"whitesmoke", 245, 245, 245),
    (b"x11 gray", 190, 190, 190),
    (b"x11 green", 0, 255, 0),
    (b"x11 grey", 190, 190, 190),
    (b"x11 maroon", 176, 48, 96),
    (b"x11 purple", 160, 32, 240),
    (b"x11gray", 190, 190, 190),
    (b"x11green", 0, 255, 0),
    (b"x11grey", 190, 190, 190),
    (b"x11maroon", 176, 48, 96),
    (b"x11purple", 160, 32, 240),
    (b"yellow", 255, 255, 0),
    (b"yellow green", 154, 205, 50),
    (b"yellow1", 255, 255, 0),
    (b"yellow2", 238, 238, 0),
    (b"yellow3", 205, 205, 0),
    (b"yellow4", 139, 139, 0),
    (b"yellowgreen", 154, 205, 50),
];

#[cfg(test)]
mod tests {
    use super::*;

    const fn c(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    fn p(s: &str) -> Result<Rgb, InvalidFormat> {
        parse(s.as_bytes())
    }

    // color.zig:873 "palette: default"
    #[test]
    fn palette_default() {
        assert_eq!(DEFAULT_PALETTE[..16], BASE16);
        // The cube and the ramp, spot checked against xterm's 256-colour table.
        assert_eq!(DEFAULT_PALETTE[16], c(0, 0, 0));
        assert_eq!(DEFAULT_PALETTE[17], c(0, 0, 95));
        assert_eq!(DEFAULT_PALETTE[196], c(255, 0, 0));
        assert_eq!(DEFAULT_PALETTE[231], c(255, 255, 255));
        assert_eq!(DEFAULT_PALETTE[232], c(8, 8, 8));
        assert_eq!(DEFAULT_PALETTE[255], c(238, 238, 238));
    }

    // color.zig:883 "RGB.parse"
    #[test]
    fn rgb_parse() {
        assert_eq!(p("rgbi:1.0/0/0"), Ok(c(255, 0, 0)));
        assert_eq!(p("rgb:7f/a0a0/0"), Ok(c(127, 160, 0)));
        assert_eq!(p("rgb:f/ff/fff"), Ok(c(255, 255, 255)));
        assert_eq!(p("#ffffff"), Ok(c(255, 255, 255)));
        assert_eq!(p("#fff"), Ok(c(255, 255, 255)));
        assert_eq!(p("#fffffffff"), Ok(c(255, 255, 255)));
        assert_eq!(p("#ffffffffffff"), Ok(c(255, 255, 255)));
        assert_eq!(p("#ff0010"), Ok(c(255, 0, 16)));
        assert_eq!(p("0A0B0C"), Ok(c(10, 11, 12)));
        assert_eq!(p("FFFFFF"), Ok(c(255, 255, 255)));
        assert_eq!(p("FFF"), Ok(c(255, 255, 255)));
        assert_eq!(p("#345"), Ok(c(51, 68, 85)));
        assert_eq!(p(" #AABBCC   "), Ok(c(170, 187, 204)));

        assert_eq!(p("black"), Ok(c(0, 0, 0)));
        assert_eq!(p("red"), Ok(c(255, 0, 0)));
        assert_eq!(p("green"), Ok(c(0, 255, 0)));
        assert_eq!(p("blue"), Ok(c(0, 0, 255)));
        assert_eq!(p("white"), Ok(c(255, 255, 255)));

        assert_eq!(p("LawnGreen"), Ok(c(124, 252, 0)));
        assert_eq!(p("medium spring green"), Ok(c(0, 250, 154)));
        assert_eq!(p(" Forest Green "), Ok(c(34, 139, 34)));
        assert_eq!(p("\tForestGreen\t"), Ok(c(34, 139, 34)));

        for bad in [
            "",
            "  ",
            "rgb;",
            "rgb:",
            ":a/a/a",
            "a/a/a",
            "rgb:a/a/a/",
            "rgb:00000///",
            "rgb:000/",
            "rgbi:a/a/a",
            "rgb:0.5/0.0/1.0",
            "rgb:not/hex/zz",
            "#",
            "#ff",
            "#ffff",
            "#fffff",
            "#gggggg",
            "#12345",
            "12345",
            "nosuchcolor",
        ] {
            assert_eq!(p(bad), Err(InvalidFormat), "{bad:?}");
        }
    }

    // Beyond ghostty's tests: what its parser accepts by way of Zig's integer and float parsers.
    #[test]
    fn rgb_parse_edges() {
        // parseUnsigned skips an inner underscore: "f_f" is 0xff at 12 bits.
        assert_eq!(p("rgb:f_f/0/0"), Ok(c(15, 0, 0)));
        assert_eq!(p("rgb:_f/0/0"), Err(InvalidFormat));
        assert_eq!(p("rgbi:0.5/0/1"), Ok(c(127, 0, 255)));
        assert_eq!(p("rgbi:nan/0/0"), Err(InvalidFormat));
        assert_eq!(p("rgbi:1.5/0/0"), Err(InvalidFormat));
        assert_eq!(p("rgb:ffff/0/0"), Ok(c(255, 0, 0)));
        assert_eq!(p("rgb:fffff/0/0"), Err(InvalidFormat));
        assert_eq!(p("rgb:+f/0/0"), Err(InvalidFormat));
    }

    // color.zig:934 "RGB: encode"
    #[test]
    fn rgb_encode() {
        let v = c(0x01, 0x23, 0xff);
        assert_eq!(&rgb8(v), b"rgb:01/23/ff");
        assert_eq!(&rgb16(v), b"rgb:0101/2323/ffff");
    }

    // color.zig:947 "DynamicPalette: init"
    #[test]
    fn dynamic_palette_init() {
        let p = DynamicPalette::new(DEFAULT_PALETTE);
        assert_eq!(p.current, DEFAULT_PALETTE);
        assert_eq!(p.original, DEFAULT_PALETTE);
        assert_eq!(p.mask.count(), 0);
    }

    // color.zig:956 "DynamicPalette: set"
    #[test]
    fn dynamic_palette_set() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        let new = c(255, 0, 0);
        p.set(0, new);
        assert_eq!(p.current[0], new);
        assert!(p.mask.is_set(0));
        assert_eq!(p.mask.count(), 1);
        assert_eq!(p.original[0], DEFAULT_PALETTE[0]);
    }

    // color.zig:970 "DynamicPalette: reset"
    #[test]
    fn dynamic_palette_reset() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        p.set(0, c(255, 0, 0));
        assert!(p.mask.is_set(0));
        p.reset(0);
        assert_eq!(p.current[0], DEFAULT_PALETTE[0]);
        assert!(!p.mask.is_set(0));
        assert_eq!(p.mask.count(), 0);
    }

    // color.zig:985 "DynamicPalette: resetAll"
    #[test]
    fn dynamic_palette_reset_all() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        for i in [0, 5, 10] {
            p.set(i, c(255, 0, 0));
        }
        assert_eq!(p.mask.count(), 3);
        p.reset_all();
        assert_eq!(p.current, DEFAULT_PALETTE);
        assert_eq!(p.original, DEFAULT_PALETTE);
        assert_eq!(p.mask.count(), 0);
    }

    // OSC 104 with no index resets only the masked entries (stream_terminal.zig:716).
    #[test]
    fn dynamic_palette_reset_modified() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        let mut changed_default = DEFAULT_PALETTE;
        changed_default[7] = c(1, 2, 3);
        p.change_default(changed_default);
        p.set(3, c(9, 9, 9));
        assert!(p.reset_modified());
        assert_eq!(p.current, changed_default);
        assert!(!p.reset_modified());
    }

    // color.zig:1002 "DynamicPalette: changeDefault with no changes"
    #[test]
    fn dynamic_palette_change_default_with_no_changes() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        let mut new = DEFAULT_PALETTE;
        new[0] = c(100, 100, 100);
        p.change_default(new);
        assert_eq!(p.original, new);
        assert_eq!(p.current, new);
        assert_eq!(p.mask.count(), 0);
    }

    // color.zig:1015 "DynamicPalette: changeDefault preserves changes"
    #[test]
    fn dynamic_palette_change_default_preserves_changes() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        let custom = c(255, 0, 0);
        p.set(5, custom);
        let mut new = DEFAULT_PALETTE;
        new[0] = c(100, 100, 100);
        new[5] = c(50, 50, 50);
        p.change_default(new);
        assert_eq!(p.original, new);
        assert_eq!(p.current[0], new[0]);
        assert_eq!(p.current[5], custom);
        assert!(p.mask.is_set(5));
        assert_eq!(p.mask.count(), 1);
    }

    // color.zig:1037 "DynamicPalette: changeDefault with multiple changes"
    #[test]
    fn dynamic_palette_change_default_with_multiple_changes() {
        let mut p = DynamicPalette::new(DEFAULT_PALETTE);
        let (red, green, blue) = (c(255, 0, 0), c(0, 255, 0), c(0, 0, 255));
        p.set(1, red);
        p.set(2, green);
        p.set(3, blue);
        let mut new = DEFAULT_PALETTE;
        new[0] = c(50, 50, 50);
        new[1] = c(60, 60, 60);
        p.change_default(new);
        assert_eq!(p.current[0], new[0]);
        assert_eq!(p.current[1], red);
        assert_eq!(p.current[2], green);
        assert_eq!(p.current[3], blue);
        assert_eq!(p.mask.count(), 3);
    }

    #[test]
    fn dynamic_rgb() {
        let mut d = DynamicRgb::new(c(1, 1, 1));
        assert_eq!(d.get(), Some(c(1, 1, 1)));
        d.set(c(2, 2, 2));
        assert_eq!(d.get(), Some(c(2, 2, 2)));
        d.reset();
        assert_eq!(d.get(), Some(c(1, 1, 1)));
        assert_eq!(DynamicRgb::UNSET.get(), None);
    }

    // color.zig:431 Special "osc4"
    #[test]
    fn special_osc4() {
        assert_eq!(Special::Bold.osc4(), 256);
        assert_eq!(Special::Underline.osc4(), 257);
        assert_eq!(Special::Blink.osc4(), 258);
        assert_eq!(Special::Reverse.osc4(), 259);
        assert_eq!(Special::Italic.osc4(), 260);
    }

    // color.zig:472 Dynamic "next"
    #[test]
    fn dynamic_next() {
        use Dynamic::*;
        assert_eq!(Foreground.next(), Some(Background));
        assert_eq!(Background.next(), Some(Cursor));
        assert_eq!(Cursor.next(), Some(PointerForeground));
        assert_eq!(PointerForeground.next(), Some(PointerBackground));
        assert_eq!(PointerBackground.next(), Some(TektronixForeground));
        assert_eq!(TektronixForeground.next(), Some(TektronixBackground));
        assert_eq!(TektronixBackground.next(), Some(HighlightBackground));
        assert_eq!(HighlightBackground.next(), Some(TektronixCursor));
        assert_eq!(TektronixCursor.next(), Some(HighlightForeground));
        assert_eq!(HighlightForeground.next(), None);
    }

    // x11_color.zig:81 (the unnamed test)
    #[test]
    fn x11_map() {
        assert_eq!(x11(b"nosuchcolor"), None);
        assert_eq!(x11(b"white"), Some(c(255, 255, 255)));
        assert_eq!(x11(b"medium spring green"), Some(c(0, 250, 154)));
        assert_eq!(x11(b"ForestGreen"), Some(c(34, 139, 34)));
        assert_eq!(x11(b"FoReStGReen"), Some(c(34, 139, 34)));
        assert_eq!(x11(b"black"), Some(c(0, 0, 0)));
        assert_eq!(x11(b"red"), Some(c(255, 0, 0)));
        assert_eq!(x11(b"green"), Some(c(0, 255, 0)));
        assert_eq!(x11(b"blue"), Some(c(0, 0, 255)));
        assert_eq!(x11(b"lawngreen"), Some(c(124, 252, 0)));
        assert_eq!(x11(b"mediumspringgreen"), Some(c(0, 250, 154)));
        assert_eq!(x11(b"forestgreen"), Some(c(34, 139, 34)));
    }

    // x11_color.zig:98 "entries": every rgb.txt line (ghostty's copy, in testdata) is found
    // with its colour, the table is sorted and holds nothing else.
    #[test]
    fn x11_entries() {
        let data = include_str!("../testdata/x11-rgb.txt");
        let mut n = 0;
        for line in data.lines().filter(|l| !l.is_empty()) {
            let num = |s: &str| s.trim().parse::<u8>().unwrap();
            let want = c(num(&line[0..3]), num(&line[4..7]), num(&line[8..11]));
            let name = line[12..].trim_matches([' ', '\t']);
            assert_eq!(x11(name.as_bytes()), Some(want), "{name}");
            n += 1;
        }
        assert!(n > 700);
        assert_eq!(n, X11.len(), "rgb.txt has no case-only duplicates");
        assert!(X11.windows(2).all(|w| w[0].0 < w[1].0), "sorted and distinct");
        assert!(X11.iter().all(|(n, ..)| !n.iter().any(u8::is_ascii_uppercase)));
    }

    #[test]
    fn zig_ints() {
        assert_eq!(zig_parse_int(b"-0", 0, 511), Some(0));
        assert_eq!(zig_parse_int(b"+5", 0, 511), Some(5));
        assert_eq!(zig_parse_int(b"-5", 0, 511), None);
        assert_eq!(zig_parse_int(b"-1", i64::from(i32::MIN), i64::from(i32::MAX)), Some(-1));
        assert_eq!(
            zig_parse_int(b"-2147483648", i64::from(i32::MIN), i64::from(i32::MAX)),
            Some(-2147483648)
        );
        assert_eq!(zig_parse_int(b"2147483648", i64::from(i32::MIN), i64::from(i32::MAX)), None);
        assert_eq!(zig_parse_int(b"+", 0, 9), None);
        assert_eq!(zig_parse_unsigned(b"65_535", 10, 65535), Some(65535));
        assert_eq!(zig_parse_unsigned(b"65536", 10, 65535), None);
        assert_eq!(zig_parse_unsigned(b"+0", 10, 9), None);
    }

    // Property: a colour written with rgb16 (and rgb8) parses back to itself.
    #[test]
    fn encode_round_trips() {
        blaze_sim::check(blaze_sim::Config::cases(300), |tape| {
            let v = c(
                tape.int_in(0..=255, "r") as u8,
                tape.int_in(0..=255, "g") as u8,
                tape.int_in(0..=255, "b") as u8,
            );
            assert_eq!(parse(&rgb16(v)), Ok(v));
            assert_eq!(parse(&rgb8(v)), Ok(v));
        });
    }
}
