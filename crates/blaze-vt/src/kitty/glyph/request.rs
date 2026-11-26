//! The glyph protocol request (ghostty's apc/glyph/request.zig): the APC body after `25a1;`
//! buffered, then classified by its verb. Options are read when asked for, as ghostty does.

use blaze_own::{AVec, Alloc};

use super::glyf::{Entry, GlyfError, Outline};
use crate::kitty::num;

/// The most bytes a decoded `glyf` payload may have (`max_payload_size`, the spec's 64 KiB).
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// Why a request is refused before it runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestError {
    /// No `verb;` layout, an unknown verb, or a register without its payload separator.
    InvalidFormat,
    /// The buffer reached its cap, or the allocator refused.
    OutOfMemory,
}

/// The bytes of one request, buffered up to a cap (`CommandParser`).
#[derive(Debug)]
pub struct Parser<'a> {
    data: AVec<'a, u8>,
    max_bytes: usize,
}

impl<'a> Parser<'a> {
    pub fn new(max_bytes: usize, alloc: Alloc<'a>) -> Parser<'a> {
        Parser { data: AVec::new_in(alloc), max_bytes }
    }

    /// One byte (`feed`): refused once `max_bytes` are held.
    pub fn feed(&mut self, byte: u8) -> Result<(), RequestError> {
        if self.data.len() >= self.max_bytes {
            return Err(RequestError::OutOfMemory);
        }
        self.data.try_push(byte).map_err(|_| RequestError::OutOfMemory)
    }

    /// Many bytes (`feedSlice`): refused whole if they would pass `max_bytes`.
    pub fn feed_slice(&mut self, bytes: &[u8]) -> Result<(), RequestError> {
        if self.data.len() + bytes.len() > self.max_bytes {
            return Err(RequestError::OutOfMemory);
        }
        self.data.try_extend_from_slice(bytes).map_err(|_| RequestError::OutOfMemory)
    }

    /// The request (`complete`). A bare one-byte verb such as `s` reads as `s;`.
    pub fn complete(mut self) -> Result<Request<'a>, RequestError> {
        if self.data.len() == 1 {
            self.data.try_push(b';').map_err(|_| RequestError::OutOfMemory)?;
        }
        Request::parse(self.data)
    }
}

/// A classified request.
#[derive(Debug)]
pub enum Request<'a> {
    /// `s`: which payload formats the terminal takes.
    Support,
    /// `q`: whether a code point is covered.
    Query(Query<'a>),
    /// `r`: register a glyph.
    Register(Register<'a>),
    /// `c`: clear one registration or all of them.
    Clear(Clear<'a>),
}

impl<'a> Request<'a> {
    /// `Request.parse`.
    fn parse(raw: AVec<'a, u8>) -> Result<Request<'a>, RequestError> {
        if raw.len() < 2 || raw[1] != b';' {
            return Err(RequestError::InvalidFormat);
        }
        Ok(match raw[0] {
            b's' => Request::Support,
            b'q' => Request::Query(Query { raw }),
            b'r' => Request::Register(Register::new(raw).ok_or(RequestError::InvalidFormat)?),
            b'c' => Request::Clear(Clear { raw }),
            _ => return Err(RequestError::InvalidFormat),
        })
    }
}

/// `q;...`.
#[derive(Debug)]
pub struct Query<'a> {
    raw: AVec<'a, u8>,
}

impl Query<'_> {
    /// `cp`, hexadecimal.
    pub fn cp(&self) -> Option<u32> {
        option_value(&self.raw[2..], b"cp").and_then(read_cp)
    }
}

/// `c;...`.
#[derive(Debug)]
pub struct Clear<'a> {
    raw: AVec<'a, u8>,
}

impl Clear<'_> {
    /// `cp`, hexadecimal; `None` when absent or not a code point.
    pub fn cp(&self) -> Option<u32> {
        option_value(&self.raw[2..], b"cp").and_then(read_cp)
    }

    /// Whether `cp=` is present at all, valid or not (`has(.cp)`).
    pub fn has_cp(&self) -> bool {
        option_value(&self.raw[2..], b"cp").is_some()
    }
}

/// `r;options;payload`: the payload follows the last `;`.
#[derive(Debug)]
pub struct Register<'a> {
    raw: AVec<'a, u8>,
    payload_idx: usize,
}

/// `fmt`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    Glyf,
    Colrv0,
    Colrv1,
}

/// `reply`: which outcomes are answered.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Reply {
    /// `0`.
    None,
    /// `1`.
    All,
    /// `2`.
    Failures,
}

/// `width`: cells the glyph takes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Width {
    Narrow,
    Wide,
}

/// `size`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Size {
    Height,
    Advance,
    Contain,
    Cover,
    Stretch,
}

/// `align`'s horizontal part.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Horizontal {
    Start,
    Center,
    End,
}

/// `align`'s vertical part.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Vertical {
    Start,
    Center,
    End,
    Baseline,
}

/// `align=h,v`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Align {
    pub horizontal: Horizontal,
    pub vertical: Vertical,
}

impl Align {
    pub const DEFAULT: Align = Align { horizontal: Horizontal::Center, vertical: Vertical::Center };
}

/// `pad=t,r,b,l`: fractions of the cell.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Pad {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Pad {
    pub const NONE: Pad = Pad { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };
}

/// Why a register payload yields no outline (`DecodeError`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayloadError {
    OutOfMemory,
    /// The decoded payload passes [`MAX_PAYLOAD_SIZE`].
    PayloadTooLarge,
    /// Not base64, or not a well-formed glyph.
    MalformedPayload,
    CompositeUnsupported,
    HintingUnsupported,
}

impl<'a> Register<'a> {
    /// `Register.init`: `r;`, then at least one more `;`.
    fn new(raw: AVec<'a, u8>) -> Option<Register<'a>> {
        if raw.len() < 2 || raw[0] != b'r' || raw[1] != b';' {
            return None;
        }
        let payload_idx = raw.iter().rposition(|&b| b == b';')?;
        if payload_idx <= 1 {
            return None;
        }
        Some(Register { raw, payload_idx })
    }

    fn options(&self) -> &[u8] {
        &self.raw[2..self.payload_idx]
    }

    fn value(&self, key: &[u8]) -> Option<&[u8]> {
        option_value(self.options(), key)
    }

    /// The base64 payload after the last `;`.
    pub fn payload(&self) -> &[u8] {
        &self.raw[self.payload_idx + 1..]
    }

    /// `cp`: no default.
    pub fn cp(&self) -> Option<u32> {
        self.value(b"cp").and_then(read_cp)
    }

    /// `fmt`, default `glyf`.
    pub fn fmt(&self) -> Option<Format> {
        match self.value(b"fmt") {
            None => Some(Format::Glyf),
            Some(b"glyf") => Some(Format::Glyf),
            Some(b"colrv0") => Some(Format::Colrv0),
            Some(b"colrv1") => Some(Format::Colrv1),
            Some(_) => None,
        }
    }

    /// `reply`, default `1`; a value that is not `0`, `1` or `2` reads as `1` too.
    pub fn reply(&self) -> Reply {
        match self.value(b"reply") {
            Some(b"0") => Reply::None,
            Some(b"2") => Reply::Failures,
            _ => Reply::All,
        }
    }

    /// `upm`, default 1000.
    pub fn upm(&self) -> Option<u32> {
        match self.value(b"upm") {
            None => Some(1000),
            Some(v) => num::parse_u32(v).ok(),
        }
    }

    /// `aw`, default `upm`.
    pub fn aw(&self) -> Option<u32> {
        match self.value(b"aw") {
            None => self.upm(),
            Some(v) => num::parse_u32(v).ok(),
        }
    }

    /// `lh`, default `upm`.
    pub fn lh(&self) -> Option<u32> {
        match self.value(b"lh") {
            None => self.upm(),
            Some(v) => num::parse_u32(v).ok(),
        }
    }

    /// `width`, default `1`.
    pub fn width(&self) -> Option<Width> {
        match self.value(b"width") {
            None | Some(b"1") => Some(Width::Narrow),
            Some(b"2") => Some(Width::Wide),
            Some(_) => None,
        }
    }

    /// `size`, default `height`.
    pub fn size(&self) -> Option<Size> {
        Some(match self.value(b"size") {
            None | Some(b"height") => Size::Height,
            Some(b"advance") => Size::Advance,
            Some(b"contain") => Size::Contain,
            Some(b"cover") => Size::Cover,
            Some(b"stretch") => Size::Stretch,
            Some(_) => return None,
        })
    }

    /// `align`, default `center,center`.
    pub fn align(&self) -> Option<Align> {
        let Some(v) = self.value(b"align") else { return Some(Align::DEFAULT) };
        let mut it = v.split(|&b| b == b',');
        let horizontal = match it.next()? {
            b"start" => Horizontal::Start,
            b"center" => Horizontal::Center,
            b"end" => Horizontal::End,
            _ => return None,
        };
        let vertical = match it.next()? {
            b"start" => Vertical::Start,
            b"center" => Vertical::Center,
            b"end" => Vertical::End,
            b"baseline" => Vertical::Baseline,
            _ => return None,
        };
        if it.next().is_some() {
            return None;
        }
        Some(Align { horizontal, vertical })
    }

    /// `pad`, default none. A pad whose sides meet or cross (`l + r >= 1` or `t + b >= 1`)
    /// reads as none (Glyph Protocol 8.5.2).
    pub fn pad(&self) -> Option<Pad> {
        let Some(v) = self.value(b"pad") else { return Some(Pad::NONE) };
        let mut it = v.split(|&b| b == b',');
        let top = fraction(it.next()?)?;
        let right = fraction(it.next()?)?;
        let bottom = fraction(it.next()?)?;
        let left = fraction(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        if left + right >= 1.0 || top + bottom >= 1.0 {
            return Some(Pad::NONE);
        }
        Some(Pad { top, right, bottom, left })
    }

    /// The payload as a `glyf` outline (`decodeGlyfPayload`): strict padded base64, at most
    /// [`MAX_PAYLOAD_SIZE`] bytes decoded.
    pub fn decode_glyf_payload<'o>(&self, alloc: Alloc<'o>) -> Result<Outline<'o>, PayloadError> {
        let payload = self.payload();
        // `calcSizeForSlice`: a multiple of four, less the padding.
        if payload.len() % 4 != 0 {
            return Err(PayloadError::MalformedPayload);
        }
        let padding = payload.iter().rev().take(2).take_while(|&&b| b == b'=').count();
        let size = payload.len() / 4 * 3 - padding;
        if size > MAX_PAYLOAD_SIZE {
            return Err(PayloadError::PayloadTooLarge);
        }
        let mut data: AVec<'o, u8> = AVec::new_in(alloc);
        data.try_reserve(size).map_err(|_| PayloadError::OutOfMemory)?;
        for _ in 0..size {
            data.try_push(0).map_err(|_| PayloadError::OutOfMemory)?;
        }
        let n = blaze_base64::STANDARD
            .decode_into(payload, &mut data)
            .map_err(|_| PayloadError::MalformedPayload)?;
        let entry = Entry::new(&data[..n]).map_err(|_| PayloadError::MalformedPayload)?;
        entry.decode(alloc).map_err(|e| match e {
            GlyfError::OutOfMemory => PayloadError::OutOfMemory,
            GlyfError::CompositeNotSupported => PayloadError::CompositeUnsupported,
            GlyfError::InstructionsNotSupported => PayloadError::HintingUnsupported,
            GlyfError::EndOfStream
            | GlyfError::EndPointsOutOfOrder
            | GlyfError::TooManyPoints
            | GlyfError::CoordinateOverflow => PayloadError::MalformedPayload,
        })
    }
}

fn read_cp(v: &[u8]) -> Option<u32> {
    num::parse_u21_hex(v).ok()
}

/// A float in `0..=1` (`parseFraction`). Rust's float grammar stands in for Zig's
/// `parseFloat`: they differ only on hex floats and `_`, which Zig takes and this refuses.
fn fraction(v: &[u8]) -> Option<f64> {
    let x: f64 = core::str::from_utf8(v).ok()?.parse().ok()?;
    (0.0..=1.0).contains(&x).then_some(x)
}

/// The value of the last `key=value` among `;`-separated options (`optionValue`).
fn option_value<'r>(raw: &'r [u8], key: &[u8]) -> Option<&'r [u8]> {
    if raw.is_empty() {
        return None;
    }
    raw.split(|&b| b == b';')
        .filter_map(|seg| {
            let eq = seg.iter().position(|&b| b == b'=')?;
            (&seg[..eq] == key).then(|| &seg[eq + 1..])
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::storage::tests::alloc;

    pub(crate) fn parse(data: &[u8]) -> Result<Request<'static>, RequestError> {
        let mut p = Parser::new(1024 * 1024, alloc());
        for &b in data {
            p.feed(b)?;
        }
        p.complete()
    }

    fn register(data: &[u8]) -> Register<'static> {
        match parse(data).unwrap() {
            Request::Register(r) => r,
            other => panic!("not a register: {other:?}"),
        }
    }

    fn clear(data: &[u8]) -> Clear<'static> {
        match parse(data).unwrap() {
            Request::Clear(c) => c,
            other => panic!("not a clear: {other:?}"),
        }
    }

    // ghostty request.zig:601 "support command"
    #[test]
    fn support_command() {
        assert!(matches!(parse(b"s"), Ok(Request::Support)));
    }

    // ghostty request.zig:610 "query command"
    #[test]
    fn query_command() {
        let Ok(Request::Query(q)) = parse(b"q;cp=E0A0") else { panic!() };
        assert_eq!(q.cp(), Some(0xE0A0));
    }

    // ghostty request.zig:620 "register command with payload"
    #[test]
    fn register_command_with_payload() {
        let r = register(b"r;cp=e0a0;fmt=glyf;upm=1000;reply=2;QQ==");
        assert_eq!(r.cp(), Some(0xE0A0));
        assert_eq!(r.fmt(), Some(Format::Glyf));
        assert_eq!(r.upm(), Some(1000));
        assert_eq!(r.reply(), Reply::Failures);
        assert_eq!(r.payload(), b"QQ==");
    }

    // ghostty request.zig:637 "register command with sizing and placement options"
    #[test]
    fn register_command_with_sizing_and_placement_options() {
        let r = register(b"r;cp=e0a0;upm=2048;aw=1024;lh=1536;width=2;size=contain;align=end,baseline;pad=0.1,0.2,0.3,0.4;QQ==");
        assert_eq!((r.upm(), r.aw(), r.lh()), (Some(2048), Some(1024), Some(1536)));
        assert_eq!(r.width(), Some(Width::Wide));
        assert_eq!(r.size(), Some(Size::Contain));
        assert_eq!(
            r.align(),
            Some(Align { horizontal: Horizontal::End, vertical: Vertical::Baseline })
        );
        assert_eq!(r.pad(), Some(Pad { top: 0.1, right: 0.2, bottom: 0.3, left: 0.4 }));
        assert_eq!(r.payload(), b"QQ==");
    }

    // ghostty request.zig:665 "register option defaults"
    #[test]
    fn register_option_defaults() {
        let r = register(b"r;;QQ==");
        assert_eq!(r.cp(), None);
        assert_eq!(r.fmt(), Some(Format::Glyf));
        assert_eq!(r.upm(), Some(1000));
        assert_eq!(r.width(), Some(Width::Narrow));
        assert_eq!(r.size(), Some(Size::Height));
        assert_eq!(r.align(), Some(Align::DEFAULT));
        assert_eq!(r.pad(), Some(Pad::NONE));
        assert_eq!(r.reply(), Reply::All);
    }

    // ghostty request.zig:681 "register command defaults"
    #[test]
    fn register_command_defaults() {
        let r = register(b"r;cp=e0a0;QQ==");
        assert_eq!(r.cp(), Some(0xE0A0));
        assert_eq!(r.fmt(), Some(Format::Glyf));
        assert_eq!((r.upm(), r.aw(), r.lh()), (Some(1000), Some(1000), Some(1000)));
        assert_eq!(r.width(), Some(Width::Narrow));
        assert_eq!(r.size(), Some(Size::Height));
        assert_eq!(r.align(), Some(Align::DEFAULT));
        assert_eq!(r.pad(), Some(Pad::NONE));
        assert_eq!(r.reply(), Reply::All);
    }

    // ghostty request.zig:700 "register command aw and lh default to upm"
    #[test]
    fn register_command_aw_and_lh_default_to_upm() {
        let r = register(b"r;cp=e0a0;upm=2048;QQ==");
        assert_eq!((r.aw(), r.lh()), (Some(2048), Some(2048)));
        // An invalid upm leaves aw and lh with nothing to default to.
        let r = register(b"r;cp=e0a0;upm=x;QQ==");
        assert_eq!((r.upm(), r.aw(), r.lh()), (None, None, None));
    }

    // ghostty request.zig:711 "register command invalid sizing and placement options"
    #[test]
    fn register_command_invalid_sizing_and_placement_options() {
        let r = register(b"r;cp=e0a0;width=3;size=invalid;align=center,middle;pad=0,1.2,0,0;QQ==");
        assert_eq!((r.width(), r.size(), r.align(), r.pad()), (None, None, None, None));
        let r = register(b"r;cp=e0a0;align=center;pad=0,0,0;fmt=svg;QQ==");
        assert_eq!((r.align(), r.pad(), r.fmt()), (None, None, None));
        let r = register(b"r;cp=e0a0;align=start,end,end;pad=0,0,0,0,0;QQ==");
        assert_eq!((r.align(), r.pad()), (None, None));
    }

    // ghostty request.zig:727 "register command degenerate padding defaults to no padding"
    #[test]
    fn register_command_degenerate_padding_defaults_to_no_padding() {
        assert_eq!(register(b"r;cp=e0a0;pad=0.4,0.2,0.6,0.1;QQ==").pad(), Some(Pad::NONE));
        assert_eq!(register(b"r;cp=e0a0;pad=0,0.5,0,0.5;QQ==").pad(), Some(Pad::NONE));
    }

    // ghostty request.zig:740 "register command invalid reply falls back to reply=1"
    #[test]
    fn register_command_invalid_reply_falls_back_to_reply_1() {
        assert_eq!(register(b"r;cp=e0a0;reply=9;QQ==").reply(), Reply::All);
        assert_eq!(register(b"r;cp=e0a0;reply=00;QQ==").reply(), Reply::All);
        assert_eq!(register(b"r;cp=e0a0;reply=0;QQ==").reply(), Reply::None);
    }

    // ghostty request.zig:750 "register command duplicate options use the last value"
    #[test]
    fn register_command_duplicate_options_use_the_last_value() {
        assert_eq!(register(b"r;cp=e0a0;reply=1;reply=2;QQ==").reply(), Reply::Failures);
    }

    // ghostty request.zig:760 "register command with invalid payload"
    #[test]
    fn register_command_with_invalid_payload() {
        let r = register(b"r;cp=e0a0;fmt=glyf;%%%not-base64%%%");
        assert_eq!((r.cp(), r.fmt()), (Some(0xE0A0), Some(Format::Glyf)));
        assert_eq!(r.payload(), b"%%%not-base64%%%");
    }

    // ghostty request.zig:775 "register command rejects missing payload separator"
    #[test]
    fn register_command_rejects_missing_payload_separator() {
        for data in [&b"r"[..], b"r;cp=e0a0", b"r;foo"] {
            assert_eq!(parse(data).unwrap_err(), RequestError::InvalidFormat, "{data:?}");
        }
    }

    // ghostty request.zig:786 "register decodes glyf payload"
    #[test]
    fn register_decodes_glyf_payload() {
        let outline = register(b"r;cp=e0a0;AAAAAAAAAAAAAA==").decode_glyf_payload(alloc()).unwrap();
        assert_eq!((outline.points.len(), outline.contours.len()), (0, 0));
    }

    // ghostty request.zig:799 "register rejects malformed glyf payload"
    #[test]
    fn register_rejects_malformed_glyf_payload() {
        let r = register(b"r;cp=e0a0;%%%not-base64%%%");
        assert_eq!(r.decode_glyf_payload(alloc()).unwrap_err(), PayloadError::MalformedPayload);
        // Valid base64 that is no glyph, and a length no multiple of four.
        assert_eq!(
            register(b"r;;QQ==").decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::MalformedPayload
        );
        assert_eq!(
            register(b"r;;QQ=").decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::MalformedPayload
        );
        assert_eq!(
            register(b"r;;").decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::MalformedPayload
        );
    }

    #[test]
    fn register_payload_size_cap() {
        let mut data = std::vec::Vec::from(&b"r;;"[..]);
        data.resize(3 + (MAX_PAYLOAD_SIZE / 3 + 1) * 4, b'A');
        assert_eq!(
            register(&data).decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::PayloadTooLarge
        );
        // Hinted and composite glyphs map to their own reasons.
        let hinted = b"r;;AAEAAAAAAAAAAAAAAAI="; // 1 contour, end 0, 2 instruction bytes
        assert_eq!(
            register(hinted).decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::HintingUnsupported
        );
        let composite = b"r;;//8AAAAAAAAAAA=="; // numberOfContours -1
        assert_eq!(
            register(composite).decode_glyf_payload(alloc()).unwrap_err(),
            PayloadError::CompositeUnsupported
        );
    }

    // ghostty request.zig:808 "register response without payload"
    #[test]
    fn register_response_without_payload() {
        let r = register(b"r;cp=E0A0;status=4;reason=out_of_namespace");
        assert_eq!(r.cp(), Some(0xE0A0));
        assert_eq!(r.payload(), b"reason=out_of_namespace");
    }

    // ghostty request.zig:824 "clear command"
    #[test]
    fn clear_command() {
        let c = clear(b"c;cp=e0a0");
        assert!(c.has_cp());
        assert_eq!(c.cp(), Some(0xE0A0));
        let c = clear(b"c");
        assert!(!c.has_cp());
    }

    // ghostty request.zig:835 "clear command tracks malformed cp presence"
    #[test]
    fn clear_command_tracks_malformed_cp_presence() {
        for data in [&b"c;cp=zz"[..], b"c;cp=", b"c;cp=200000"] {
            let c = clear(data);
            assert!(c.has_cp());
            assert_eq!(c.cp(), None);
        }
    }

    // ghostty request.zig:848 "invalid command"
    #[test]
    fn invalid_command() {
        assert_eq!(parse(b"x").unwrap_err(), RequestError::InvalidFormat);
        assert_eq!(parse(b"").unwrap_err(), RequestError::InvalidFormat);
        assert_eq!(parse(b"sx").unwrap_err(), RequestError::InvalidFormat);
    }

    #[test]
    fn parser_caps() {
        let mut p = Parser::new(3, alloc());
        p.feed_slice(b"q;c").unwrap();
        assert_eq!(p.feed(b'p'), Err(RequestError::OutOfMemory));
        let mut p = Parser::new(3, alloc());
        p.feed(b'q').unwrap();
        assert_eq!(p.feed_slice(b";cp"), Err(RequestError::OutOfMemory));
        p.feed_slice(b";c").unwrap();
    }
}
