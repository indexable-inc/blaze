//! Every reply's exact bytes and terminator (vt-inventory.md §12), as pure encoders. Each takes
//! the terminal state it reports as plain arguments and appends to the caller's buffer; on an
//! allocation refusal the buffer is left as it was (no half reply) and the
//! [`AllocFailure`] returned. Nothing here decides *whether* to reply (a mode, a permission,
//! a missing effect): the caller does, and the doc of each encoder says what ghostty checks.
//!
//! | trigger | encoder | ghostty |
//! |---|---|---|
//! | DA1 `CSI c`, `ESC Z` | [`da1`] | device_attributes.zig:41, SH:834 |
//! | DA2 `CSI > c` | [`da2`] | device_attributes.zig:80 |
//! | DA3 `CSI = c` | [`da3`] | device_attributes.zig:104 (lib only; the app sends none) |
//! | DSR 5 | [`operating_status`] | ST:384 |
//! | DSR 6 (CPR) | [`cursor_position`] | ST:386 |
//! | `CSI ? 996 n`, mode 2031 | [`color_scheme`] | device_status.zig:26 |
//! | DECRQM | [`mode_report`] | modes.zig:207 |
//! | DECRQSS | [`decrqss`] | SH:478, T:3416 |
//! | XTGETTCAP | [`xtgettcap`] | SH:470, Source.zig:71 |
//! | XTVERSION | [`xtversion`] | ST:429 |
//! | `CSI 14/16/18 t`, mode 2048 | [`size_report`] | size_report.zig:43 |
//! | `CSI 21 t` | [`title_report`] | ST:440 |
//! | `CSI ? u` | [`kitty_keyboard`] | ST:553 |
//! | OSC 4/10/11/12 query | [`osc_color`] | ST:743, SH:1393 |
//! | OSC 21 query | [`osc21`] | ST:824, SH:1484 |
//! | OSC 52 read | [`osc52`] | Surface.zig:5935 |
//! | ENQ | [`enquiry`] | ST:416 |
//!
//! DECXCPR (`CSI ? 6 n`) has no encoder: ghostty sends no reply to it (inventory, "Deliberately
//! NOT supported").

#[cfg(test)]
mod tests;

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::dcs::{Decrqss, XtGetTcap};
pub use crate::osc::Clipboard;
use crate::parser::Terminator;
pub use crate::screen::CursorShape;
use crate::style::{Color, Rgb, Style, Underline};
use crate::terminfo::{self, Cap};

/// The buffer replies go to.
pub type Out<'v, 'a> = &'v mut Vec<u8, Alloc<'a>>;

/// Appends what `f` writes, or nothing if any piece is refused.
fn emit(out: Out<'_, '_>, f: impl FnOnce(&mut W<'_, '_>)) -> Result<(), AllocFailure> {
    let start = out.len();
    let mut w = W { out, failed: None };
    f(&mut w);
    match w.failed {
        None => Ok(()),
        Some(why) => {
            w.out.truncate(start);
            Err(why)
        }
    }
}

/// A writer that remembers the first refusal and writes nothing after it.
struct W<'v, 'a> {
    out: &'v mut Vec<u8, Alloc<'a>>,
    failed: Option<AllocFailure>,
}

impl W<'_, '_> {
    fn bytes(&mut self, b: &[u8]) {
        if self.failed.is_none()
            && let Err(why) = self.out.try_extend_from_slice(b)
        {
            self.failed = Some(why);
        }
    }

    fn byte(&mut self, b: u8) {
        self.bytes(&[b]);
    }

    fn dec(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.bytes(&digits[i..]);
    }

    /// Upper-case hex of each byte.
    fn hex_upper(&mut self, b: u8) {
        const D: &[u8; 16] = b"0123456789ABCDEF";
        self.bytes(&[D[usize::from(b >> 4)], D[usize::from(b & 0xF)]]);
    }

    /// Lower-case hex, zero-padded to `width` digits.
    fn hex_lower(&mut self, v: u32, width: usize) {
        const D: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; 8];
        for (i, slot) in buf[8 - width..].iter_mut().enumerate() {
            *slot = D[((v >> (4 * (width - 1 - i))) & 0xF) as usize];
        }
        self.bytes(&buf[8 - width..]);
    }
}

// --- device attributes ---

/// DA1's reply: `CSI ? level ; features… c`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Da1<'f> {
    /// The conformance level (62: VT220, level 2).
    pub conformance: u16,
    /// Feature codes (22: ANSI colour, 52: clipboard).
    pub features: &'f [u16],
}

impl Da1<'static> {
    /// libghostty's default, and the app's when clipboard writes are denied: `CSI ? 62 ; 22 c`.
    pub const LIB: Da1<'static> = Da1 { conformance: 62, features: &[22] };
    /// The app's when clipboard writes are allowed: `CSI ? 62 ; 22 ; 52 c` (SH:834).
    pub const APP: Da1<'static> = Da1 { conformance: 62, features: &[22, 52] };
}

pub fn da1(out: Out<'_, '_>, da: Da1<'_>) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b[?");
        w.dec(u64::from(da.conformance));
        for &f in da.features {
            w.byte(b';');
            w.dec(u64::from(f));
        }
        w.byte(b'c');
    })
}

/// DA2's reply: `CSI > type ; firmware ; rom c`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Da2 {
    pub device_type: u16,
    pub firmware: u16,
    pub rom_cartridge: u16,
}

impl Da2 {
    /// libghostty's default: `CSI > 1 ; 0 ; 0 c`.
    pub const LIB: Da2 = Da2 { device_type: 1, firmware: 0, rom_cartridge: 0 };
    /// The app's: `CSI > 1 ; 10 ; 0 c` (SH:845).
    pub const APP: Da2 = Da2 { device_type: 1, firmware: 10, rom_cartridge: 0 };
}

pub fn da2(out: Out<'_, '_>, da: Da2) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b[>");
        w.dec(u64::from(da.device_type));
        w.byte(b';');
        w.dec(u64::from(da.firmware));
        w.byte(b';');
        w.dec(u64::from(da.rom_cartridge));
        w.byte(b'c');
    })
}

/// DA3's reply (DECRPTUI): `DCS ! | XXXXXXXX ST`, the unit id as eight upper-case hex digits.
/// libghostty sends it (unit id 0); the app sends nothing.
pub fn da3(out: Out<'_, '_>, unit_id: u32) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1bP!|");
        for b in unit_id.to_be_bytes() {
            w.hex_upper(b);
        }
        w.bytes(b"\x1b\\");
    })
}

// --- device status ---

/// DSR 5: `CSI 0 n`.
pub fn operating_status(out: Out<'_, '_>) -> Result<(), AllocFailure> {
    emit(out, |w| w.bytes(b"\x1b[0n"))
}

/// What a CPR reports: the cursor, 0-based, and the margins origin mode measures from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CursorReport {
    pub x: u32,
    pub y: u32,
    /// DECOM (mode 6).
    pub origin: bool,
    /// The scrolling region's left and top, 0-based.
    pub left: u32,
    pub top: u32,
}

/// DSR 6 (CPR): `CSI row ; col R`, 1-based; in origin mode relative to the region's top-left,
/// saturating at it (ST:388).
pub fn cursor_position(out: Out<'_, '_>, c: CursorReport) -> Result<(), AllocFailure> {
    let (x, y) =
        if c.origin { (c.x.saturating_sub(c.left), c.y.saturating_sub(c.top)) } else { (c.x, c.y) };
    emit(out, |w| {
        w.bytes(b"\x1b[");
        w.dec(u64::from(y) + 1);
        w.byte(b';');
        w.dec(u64::from(x) + 1);
        w.byte(b'R');
    })
}

/// The colour scheme a `CSI ? 996 n` reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// `CSI ? 997 ; 1 n` (dark) or `CSI ? 997 ; 2 n` (light): the reply to `CSI ? 996 n`, and
/// what is sent unsolicited on a change while mode 2031 is set.
pub fn color_scheme(out: Out<'_, '_>, scheme: ColorScheme) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(match scheme {
            ColorScheme::Dark => b"\x1b[?997;1n",
            ColorScheme::Light => b"\x1b[?997;2n",
        });
    })
}

// --- modes ---

/// A mode's state in a DECRPM (modes.zig:197 `Report.State`; the modes themselves are
/// [`crate::terminal::ModeState`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportState {
    NotRecognized = 0,
    Set = 1,
    Reset = 2,
    PermanentlySet = 3,
    PermanentlyReset = 4,
}

/// A DECRPM: which mode, which form it was asked in, its state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModeReport {
    pub mode: u16,
    /// Asked as `CSI n $ p` (ANSI) rather than `CSI ? n $ p` (DEC).
    pub ansi: bool,
    pub state: ReportState,
}

/// DECRPM: `CSI ? n ; s $ y`, or `CSI n ; s $ y` for the ANSI form. ghostty's mode tag holds
/// 15 bits, so the number is truncated to them (ST:535 `@truncate`): `?40000` reports 7232.
/// (ghostty's stream gives the ANSI form no reply; that is the caller's choice.)
pub fn mode_report(out: Out<'_, '_>, r: ModeReport) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(if r.ansi { b"\x1b[" } else { b"\x1b[?" });
        w.dec(u64::from(r.mode & 0x7FFF));
        w.byte(b';');
        w.dec(r.state as u64);
        w.bytes(b"$y");
    })
}

// --- DECRQSS ---

/// What DECRQSS reports from: the active screen's pen, region and cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecrqssState<'s> {
    /// The cursor's style (the pen SGR sets).
    pub style: &'s Style,
    /// The scrolling region, 0-based, inclusive.
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
    /// DECLRMM (mode 69): DECSLRM is reported only while it is set.
    pub left_right_margins: bool,
    /// The cursor's shape; a hollow block (unfocused), which DECSCUSR cannot name, is reported
    /// as a block.
    pub cursor_shape: CursorShape,
    /// Mode 12.
    pub cursor_blink: bool,
}

/// DECRPSS: `DCS 1 $ r payload ST`, or `DCS 0 $ r ST` when there is no payload (an unknown
/// setting, or DECSLRM without mode 69) (SH:478).
pub fn decrqss(out: Out<'_, '_>, req: Decrqss, s: &DecrqssState<'_>) -> Result<(), AllocFailure> {
    let valid = match req {
        Decrqss::None => false,
        Decrqss::Decslrm => s.left_right_margins,
        Decrqss::Sgr | Decrqss::Decscusr | Decrqss::Decstbm => true,
    };
    emit(out, |w| {
        w.bytes(if valid { b"\x1bP1$r" } else { b"\x1bP0$r" });
        if valid {
            match req {
                Decrqss::None => {}
                Decrqss::Sgr => {
                    sgr_attributes(w, s.style);
                    w.byte(b'm');
                }
                Decrqss::Decscusr => {
                    let n: u64 = match s.cursor_shape {
                        CursorShape::Block | CursorShape::BlockHollow => 1,
                        CursorShape::Underline => 3,
                        CursorShape::Bar => 5,
                    };
                    w.dec(if s.cursor_blink { n } else { n + 1 });
                    w.bytes(b" q");
                }
                Decrqss::Decstbm => {
                    w.dec(u64::from(s.top) + 1);
                    w.byte(b';');
                    w.dec(u64::from(s.bottom) + 1);
                    w.byte(b'r');
                }
                Decrqss::Decslrm => {
                    w.dec(u64::from(s.left) + 1);
                    w.byte(b';');
                    w.dec(u64::from(s.right) + 1);
                    w.byte(b's');
                }
            }
        }
        w.bytes(b"\x1b\\");
    })
}

/// The SGR a DECRQSS `m` reports, without the final `m` (T:3416 `printAttributes`): `0`, then
/// `1 2 3 4 5 7 8 9` for bold, faint, italic, any underline, blink, inverse, invisible,
/// strikethrough, then the foreground and background. Underline style, overline and the
/// underline colour are not reported.
pub fn sgr_payload(out: Out<'_, '_>, style: &Style) -> Result<(), AllocFailure> {
    emit(out, |w| sgr_attributes(w, style))
}

fn sgr_attributes(w: &mut W<'_, '_>, style: &Style) {
    w.byte(b'0');
    let f = &style.flags;
    for (on, c) in [
        (f.bold, b'1'),
        (f.faint, b'2'),
        (f.italic, b'3'),
        (f.underline != Underline::None, b'4'),
        (f.blink, b'5'),
        (f.inverse, b'7'),
        (f.invisible, b'8'),
        (f.strikethrough, b'9'),
    ] {
        if on {
            w.byte(b';');
            w.byte(c);
        }
    }
    sgr_color(w, style.fg, b"3", b"9", b"38");
    sgr_color(w, style.bg, b"4", b"10", b"48");
}

fn sgr_color(w: &mut W<'_, '_>, c: Color, base: &[u8], bright: &[u8], ext: &[u8]) {
    match c {
        Color::None => {}
        Color::Palette(i) => {
            w.byte(b';');
            if i >= 16 {
                w.bytes(ext);
                w.bytes(b":5:");
                w.dec(u64::from(i));
            } else if i >= 8 {
                w.bytes(bright);
                w.dec(u64::from(i - 8));
            } else {
                w.bytes(base);
                w.dec(u64::from(i));
            }
        }
        Color::Rgb(Rgb { r, g, b }) => {
            w.byte(b';');
            w.bytes(ext);
            w.bytes(b":2::");
            w.dec(u64::from(r));
            w.byte(b':');
            w.dec(u64::from(g));
            w.byte(b':');
            w.dec(u64::from(b));
        }
    }
}

// --- XTGETTCAP ---

/// One reply per key ghostty's terminfo answers, in the order asked; a key it does not know
/// gets no reply (SH:470 `orelse continue`), so a request of only unknown keys writes nothing.
pub fn xtgettcap(out: Out<'_, '_>, req: &XtGetTcap<'_>) -> Result<(), AllocFailure> {
    emit(out, |w| {
        for key in req.keys() {
            if let Some((name, cap)) = terminfo::lookup_hex(key) {
                xtgettcap_entry(w, name, cap);
            }
        }
    })
}

/// The reply for one capability: `DCS 1 + r NAME ST` for a boolean, `DCS 1 + r NAME = VALUE ST`
/// otherwise, name and value in upper-case hex (Source.zig:128).
pub fn xtgettcap_cap(out: Out<'_, '_>, name: &str, cap: &Cap) -> Result<(), AllocFailure> {
    emit(out, |w| xtgettcap_entry(w, name, cap))
}

fn xtgettcap_entry(w: &mut W<'_, '_>, name: &str, cap: &Cap) {
    w.bytes(b"\x1bP1+r");
    for &b in name.as_bytes() {
        w.hex_upper(b);
    }
    match *cap {
        Cap::Bool => {}
        Cap::Num(n) => {
            w.byte(b'=');
            let mut digits = [0u8; 10];
            let mut i = digits.len();
            let mut v = n;
            loop {
                i -= 1;
                digits[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            for &d in &digits[i..] {
                w.hex_upper(d);
            }
        }
        Cap::Str(s) => {
            w.byte(b'=');
            terminfo::reply_value(s, |b| w.hex_upper(b));
        }
    }
    w.bytes(b"\x1b\\");
}

// --- XTVERSION ---

/// XTVERSION: `DCS > | text ST`. The app's text is `ghostty {version}`; libghostty sends the
/// host's text, or `libghostty` when that is empty (ST:429), as this does.
pub fn xtversion(out: Out<'_, '_>, text: &[u8]) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1bP>|");
        w.bytes(if text.is_empty() { b"libghostty" } else { text });
        w.bytes(b"\x1b\\");
    })
}

// --- window reports ---

/// The size a report carries: cells and one cell's pixels (size_report.zig:20).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

/// Which size report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SizeReport {
    /// In-band (mode 2048), on resize and when the mode is set:
    /// `CSI 48 ; rows ; cols ; hpx ; wpx t`.
    Mode2048,
    /// `CSI 14 t`: `CSI 4 ; hpx ; wpx t`.
    Csi14,
    /// `CSI 16 t`: `CSI 6 ; cellh ; cellw t`.
    Csi16,
    /// `CSI 18 t`: `CSI 8 ; rows ; cols t`.
    Csi18,
}

/// A size report; pixels are cells times cell size in 64 bits, never overflowing.
pub fn size_report(out: Out<'_, '_>, style: SizeReport, s: Size) -> Result<(), AllocFailure> {
    let hpx = u64::from(s.rows) * u64::from(s.cell_height);
    let wpx = u64::from(s.columns) * u64::from(s.cell_width);
    let (rows, cols) = (u64::from(s.rows), u64::from(s.columns));
    let (fields, n) = match style {
        SizeReport::Mode2048 => ([48, rows, cols, hpx, wpx], 5),
        SizeReport::Csi14 => ([4, hpx, wpx, 0, 0], 3),
        SizeReport::Csi16 => ([6, u64::from(s.cell_height), u64::from(s.cell_width), 0, 0], 3),
        SizeReport::Csi18 => ([8, rows, cols, 0, 0], 3),
    };
    emit(out, |w| {
        w.bytes(b"\x1b[");
        for (i, &f) in fields[..n].iter().enumerate() {
            if i > 0 {
                w.byte(b';');
            }
            w.dec(f);
        }
        w.byte(b't');
    })
}

/// `CSI 21 t`: `OSC l title ST` (ST:440); no title is the empty title.
pub fn title_report(out: Out<'_, '_>, title: &[u8]) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b]l");
        w.bytes(title);
        w.bytes(b"\x1b\\");
    })
}

// --- kitty keyboard ---

/// `CSI ? u`: `CSI ? flags u`, the current flags of the active screen's stack (0–31).
pub fn kitty_keyboard(out: Out<'_, '_>, flags: u8) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b[?");
        w.dec(u64::from(flags));
        w.byte(b'u');
    })
}

// --- OSC ---

/// An OSC reply's terminator: the one the query ended with (`BEL` for `BEL`, else `ST`).
pub fn terminator(out: Out<'_, '_>, t: Terminator) -> Result<(), AllocFailure> {
    emit(out, |w| put_terminator(w, t))
}

fn put_terminator(w: &mut W<'_, '_>, t: Terminator) {
    w.bytes(match t {
        Terminator::Bel => b"\x07",
        Terminator::St => b"\x1b\\",
    });
}

/// Which colour an OSC 4/10/11/12 query asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OscColor {
    /// OSC 4 with this index.
    Palette(u8),
    /// OSC 10.
    Foreground,
    /// OSC 11.
    Background,
    /// OSC 12 (the caller passes the foreground when no cursor colour is set, as ghostty does).
    Cursor,
}

/// How a colour is spelled in a reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RgbFormat {
    /// `rgb:rrrr/gggg/bbbb`, each channel times 257 (libghostty; the app's default).
    Bits16,
    /// `rgb:rr/gg/bb` (the app's `osc-color-report-format = 8-bit`).
    Bits8,
}

/// An OSC colour query's reply: `OSC 4 ; i ; rgb:… T` or `OSC 10|11|12 ; rgb:… T`. A query of
/// several colours is several replies, back to back.
pub fn osc_color(
    out: Out<'_, '_>,
    which: OscColor,
    c: Rgb,
    format: RgbFormat,
    t: Terminator,
) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b]");
        match which {
            OscColor::Palette(i) => {
                w.bytes(b"4;");
                w.dec(u64::from(i));
            }
            OscColor::Foreground => w.bytes(b"10"),
            OscColor::Background => w.bytes(b"11"),
            OscColor::Cursor => w.bytes(b"12"),
        }
        w.byte(b';');
        rgb_spec(w, c, format);
        put_terminator(w, t);
    })
}

fn rgb_spec(w: &mut W<'_, '_>, c: Rgb, format: RgbFormat) {
    w.bytes(b"rgb:");
    for (i, ch) in [c.r, c.g, c.b].into_iter().enumerate() {
        if i > 0 {
            w.byte(b'/');
        }
        match format {
            RgbFormat::Bits16 => w.hex_lower(u32::from(ch) * 257, 4),
            RgbFormat::Bits8 => w.hex_lower(u32::from(ch), 2),
        }
    }
}

/// A kitty colour protocol (OSC 21) key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Osc21Key<'k> {
    /// A palette index, written in decimal.
    Palette(u8),
    /// A special colour by its kitty name (`foreground`, `background`, `cursor`, …).
    Special(&'k str),
}

/// One answered key: its colour, or `None` for a colour that is not set (an empty value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Osc21Entry<'k> {
    pub key: Osc21Key<'k>,
    pub color: Option<Rgb>,
}

/// An OSC 21 query's reply: `OSC 21 ; key=rgb:rr/gg/bb ; key= … T`, all keys in one reply,
/// 8-bit channels; no entries, no reply (ST:824). Keys ghostty does not answer (special colours
/// other than foreground, background and cursor) are the caller's to leave out.
pub fn osc21(
    out: Out<'_, '_>,
    entries: &[Osc21Entry<'_>],
    t: Terminator,
) -> Result<(), AllocFailure> {
    if entries.is_empty() {
        return Ok(());
    }
    emit(out, |w| {
        w.bytes(b"\x1b]21");
        for e in entries {
            w.byte(b';');
            match e.key {
                Osc21Key::Palette(i) => w.dec(u64::from(i)),
                Osc21Key::Special(name) => w.bytes(name.as_bytes()),
            }
            w.byte(b'=');
            if let Some(c) = e.color {
                rgb_spec(w, c, RgbFormat::Bits8);
            }
        }
        put_terminator(w, t);
    })
}

/// An OSC 52 read's reply: `OSC 52 ; kind ; base64 ST`, always `ST` whatever ended the query
/// (Surface.zig:5953), even for empty contents. `base64` is the contents already encoded
/// (standard alphabet, padded): the clipboard owner encodes them.
pub fn osc52(out: Out<'_, '_>, kind: Clipboard, base64: &[u8]) -> Result<(), AllocFailure> {
    emit(out, |w| {
        w.bytes(b"\x1b]52;");
        w.byte(kind.letter());
        w.byte(b';');
        w.bytes(base64);
        w.bytes(b"\x1b\\");
    })
}

/// ENQ: the configured answerback, verbatim. libghostty sends nothing for an empty one or one
/// of 256 bytes or more (ST:416, its stack buffer), as this does.
pub fn enquiry(out: Out<'_, '_>, answerback: &[u8]) -> Result<(), AllocFailure> {
    if answerback.is_empty() || answerback.len() >= 256 {
        return Ok(());
    }
    emit(out, |w| w.bytes(answerback))
}
