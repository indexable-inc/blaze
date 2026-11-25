//! OSC payloads as ghostty's osc.zig and osc/parsers/*.zig parse them: [`parse`] takes the bytes
//! between `ESC ]` and the terminator and gives the [`Command`], or `None` where ghostty's parser
//! ends `invalid`. Nothing is copied: a command borrows the payload, and lists (colour
//! operations, kitty colour requests) are iterators over it.
//!
//! The capture rules are ghostty's: the bytes after the number's `;` go in a 2048-byte buffer
//! ([`MAX_BUF`]), and more invalidates the OSC; a parser that appends a NUL (titles, pwd,
//! hyperlinks, …) also fails on exactly 2048. OSC 52, 66, 72 and 5522 capture without a limit
//! ([`unbounded`] tells the capture which). [`run_colors`] applies a colour operation to
//! [`Colors`] and writes its query replies, as stream_terminal.zig's handler does.
//!
//! One deliberate change: ghostty's prefix machine accepts only `;` or `4` after `10`, so OSC 105
//! (reset special colours) never parses although its colour parser exists. Here `105` reaches
//! that parser ([`Operation::ResetSpecial`]).

use crate::color::{self, Colors, Dynamic, Rgb, Special, zig_parse_int, zig_parse_unsigned};
pub use crate::parser::Terminator;

/// The fixed capture buffer (osc.zig `Parser.MAX_BUF`).
pub const MAX_BUF: usize = 2048;

impl Terminator {
    /// The bytes that end a reply to a request with this terminator (osc.zig `Terminator.string`).
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Terminator::St => b"\x1b\\",
            Terminator::Bel => b"\x07",
        }
    }
}

/// Whether ghostty captures this OSC without a size limit (52, 66, 72, 5522 after their `;`).
/// `head` is the payload so far; the answer is settled once it holds the number and its `;`.
pub fn unbounded(head: &[u8]) -> bool {
    [&b"52;"[..], b"66;", b"72;", b"5522;"].iter().any(|p| head.starts_with(p))
}

/// A parsed OSC (osc.zig:23 `Command`). Strings are the payload's bytes, unvalidated: a title
/// that is not UTF-8 is the handler's to ignore, as ghostty's handler does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command<'a> {
    /// OSC 0 and 2.
    ChangeWindowTitle(&'a [u8]),
    /// OSC 1.
    ChangeWindowIcon(&'a [u8]),
    /// OSC 133, and OSC 9;12 (as A).
    SemanticPrompt(SemanticPrompt<'a>),
    /// OSC 52 (`kind` is the selection letter, `c` when empty; `data` is base64, or `?` to read),
    /// and OSC 1337 `Copy=:`.
    ClipboardContents { kind: u8, data: &'a [u8] },
    /// OSC 7, OSC 9;9 and OSC 1337 `CurrentDir=`.
    ReportPwd(&'a [u8]),
    /// OSC 22, the name as sent; [`MouseShape::from_name`] reads it.
    MouseShape(&'a [u8]),
    /// OSC 4, 5, 10–19, 104, 105, 110–119.
    ColorOperation(ColorOperation<'a>),
    /// OSC 21.
    KittyColorProtocol(KittyColor<'a>),
    /// OSC 9 text and OSC 777 `notify`.
    ShowDesktopNotification { title: &'a [u8], body: &'a [u8] },
    /// OSC 8 with a URI.
    HyperlinkStart { id: Option<&'a [u8]>, uri: &'a [u8] },
    /// OSC 8 with no URI.
    HyperlinkEnd,
    /// OSC 9;1.
    ConemuSleep { duration_ms: u16 },
    /// OSC 9;2.
    ConemuShowMessageBox(&'a [u8]),
    /// OSC 9;3.
    ConemuChangeTabTitle(TabTitle<'a>),
    /// OSC 9;4.
    ConemuProgressReport(ProgressReport),
    /// OSC 9;5.
    ConemuWaitInput,
    /// OSC 9;6.
    ConemuGuimacro(&'a [u8]),
    /// OSC 9;7.
    ConemuRunProcess(&'a [u8]),
    /// OSC 9;8.
    ConemuOutputEnvironmentVariable(&'a [u8]),
    /// OSC 9;10.
    ConemuXtermEmulation { keyboard: Option<bool>, output: Option<bool> },
    /// OSC 9;11.
    ConemuComment(&'a [u8]),
    /// OSC 66.
    KittyTextSizing(TextSizing<'a>),
    /// OSC 5522.
    KittyClipboardProtocol(KittyClipboard<'a>),
    /// OSC 72.
    KittyDndProtocol(KittyDnd<'a>),
    /// OSC 3008.
    ContextSignal(ContextSignal<'a>),
}

/// Which parser a number selects (osc.zig:612 `end`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Title,
    Icon,
    Color(Operation),
    Pwd,
    Hyperlink,
    Osc9,
    KittyColor,
    Mouse,
    Clipboard,
    Context,
    TextSizing,
    Dnd,
    Prompt,
    Rxvt,
    Iterm2,
    KittyClipboard,
}

fn kind(number: &[u8]) -> Option<Kind> {
    use Kind as K;
    Some(match number {
        b"0" | b"2" => K::Title,
        b"1" => K::Icon,
        b"4" => K::Color(Operation::Palette),
        b"5" => K::Color(Operation::Special),
        b"7" => K::Pwd,
        b"8" => K::Hyperlink,
        b"9" => K::Osc9,
        b"21" => K::KittyColor,
        b"22" => K::Mouse,
        b"52" => K::Clipboard,
        b"66" => K::TextSizing,
        b"72" => K::Dnd,
        b"104" => K::Color(Operation::ResetPalette),
        b"105" => K::Color(Operation::ResetSpecial),
        b"133" => K::Prompt,
        b"777" => K::Rxvt,
        b"1337" => K::Iterm2,
        b"3008" => K::Context,
        b"5522" => K::KittyClipboard,
        [b'1', d @ b'0'..=b'9'] => {
            K::Color(Operation::Dynamic(Dynamic::from_code(10 + (d - b'0'))?))
        }
        [b'1', b'1', d @ b'0'..=b'9'] => {
            K::Color(Operation::ResetDynamic(Dynamic::from_code(10 + (d - b'0'))?))
        }
        _ => return None,
    })
}

/// Parse one OSC payload: the bytes between `ESC ]` and the terminator, which the request's
/// replies reuse.
pub fn parse(payload: &[u8], terminator: Terminator) -> Option<Command<'_>> {
    let semi = payload.iter().position(|&b| b == b';');
    let number = &payload[..semi.unwrap_or(payload.len())];
    let kind = kind(number)?;
    let Some(semi) = semi else {
        // No `;`: only the colour operations run, on no data.
        return match kind {
            Kind::Color(op) => {
                Some(Command::ColorOperation(ColorOperation { op, data: b"", terminator }))
            }
            _ => None,
        };
    };
    let data = &payload[semi + 1..];
    let bounded =
        !matches!(kind, Kind::Clipboard | Kind::TextSizing | Kind::Dnd | Kind::KittyClipboard);
    if bounded && data.len() > MAX_BUF {
        return None;
    }
    // A parser that appends a NUL to the capture needs one byte of room.
    let nul = || (!bounded || data.len() < MAX_BUF).then_some(());

    match kind {
        Kind::Title => nul().map(|()| Command::ChangeWindowTitle(data)),
        Kind::Icon => nul().map(|()| Command::ChangeWindowIcon(data)),
        Kind::Pwd => nul().map(|()| Command::ReportPwd(data)),
        Kind::Mouse => nul().map(|()| Command::MouseShape(data)),
        Kind::Color(op) => Some(Command::ColorOperation(ColorOperation { op, data, terminator })),
        Kind::Clipboard => clipboard(data),
        Kind::Rxvt => {
            nul()?;
            rxvt(data)
        }
        Kind::Hyperlink => {
            nul()?;
            hyperlink(data)
        }
        Kind::Osc9 => osc9(data, nul().is_some()),
        Kind::KittyColor => KittyColor::new(data, terminator).map(Command::KittyColorProtocol),
        Kind::Prompt => SemanticPrompt::parse(data).map(Command::SemanticPrompt),
        Kind::Iterm2 => {
            nul()?;
            iterm2(data)
        }
        Kind::Context => ContextSignal::parse(data).map(Command::ContextSignal),
        Kind::TextSizing => TextSizing::parse(data).map(Command::KittyTextSizing),
        Kind::Dnd => {
            let (metadata, payload) = split_payload(data);
            Some(Command::KittyDndProtocol(KittyDnd { metadata, payload, terminator }))
        }
        Kind::KittyClipboard => {
            let (metadata, payload) = split_payload(data);
            Some(Command::KittyClipboardProtocol(KittyClipboard { metadata, payload, terminator }))
        }
    }
}

fn find(s: &[u8], b: u8) -> Option<usize> {
    s.iter().position(|&c| c == b)
}

/// `metadata;payload`, the payload absent without a `;`.
fn split_payload(data: &[u8]) -> (&[u8], Option<&[u8]>) {
    match find(data, b';') {
        Some(i) => (&data[..i], Some(&data[i + 1..])),
        None => (data, None),
    }
}

/// clipboard_operation.zig:15.
fn clipboard(data: &[u8]) -> Option<Command<'_>> {
    match data {
        [] => None,
        [b';', rest @ ..] => Some(Command::ClipboardContents { kind: b'c', data: rest }),
        [kind, b';', rest @ ..] => Some(Command::ClipboardContents { kind: *kind, data: rest }),
        _ => None,
    }
}

/// Which clipboard an OSC 52 kind names, as ghostty's handler maps it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Clipboard {
    Standard,
    Selection,
    Primary,
}

impl Clipboard {
    /// The letter a reply names it by: `c`, `s`, `p`.
    pub fn letter(self) -> u8 {
        match self {
            Clipboard::Standard => b'c',
            Clipboard::Selection => b's',
            Clipboard::Primary => b'p',
        }
    }

    pub fn from_kind(kind: u8) -> Clipboard {
        match kind {
            b's' => Clipboard::Selection,
            b'p' => Clipboard::Primary,
            _ => Clipboard::Standard,
        }
    }
}

/// rxvt_extension.zig:16: `notify;title;body`, the only extension ghostty takes.
fn rxvt(data: &[u8]) -> Option<Command<'_>> {
    let k = find(data, b';')?;
    if &data[..k] != b"notify" {
        return None;
    }
    let rest = &data[k + 1..];
    let t = find(rest, b';')?;
    Some(Command::ShowDesktopNotification { title: &rest[..t], body: &rest[t + 1..] })
}

/// hyperlink.zig:15: `params;uri`, params `:`-separated `key=value`, only `id` read.
fn hyperlink(data: &[u8]) -> Option<Command<'_>> {
    let s = find(data, b';')?;
    let uri = &data[s + 1..];
    // ghostty turns each `:` in the params, and the `;`, into a NUL and walks the pieces.
    let is_sep = |i: usize| i == s || data[i] == b':';
    let mut id = None;
    let mut kv_start = 0;
    while kv_start < s + 1 {
        let Some(kv_end) = (kv_start + 1..=s).find(|&i| is_sep(i)) else { break };
        let kv = &data[kv_start..kv_end];
        let Some(v) = find(kv, b'=') else { break };
        // A piece that starts at a separator (two in a row) has it as its first byte, as
        // ghostty's NUL, so its key never matches.
        let key = if is_sep(kv_start) { &[0u8][..] } else { &kv[..v] };
        let value = &kv[v + 1..];
        if key == b"id" && !value.is_empty() {
            id = Some(value);
        }
        kv_start = kv_end + 1;
    }
    if uri.is_empty() {
        return if id.is_some() { None } else { Some(Command::HyperlinkEnd) };
    }
    Some(Command::HyperlinkStart { id, uri })
}

/// OSC 9;3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabTitle<'a> {
    Reset,
    Value(&'a [u8]),
}

/// OSC 9;4 states (osc.zig `ProgressReport.State`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

/// OSC 9;4.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProgressReport {
    pub state: ProgressState,
    /// 0–100 where given (clamped).
    pub progress: Option<u8>,
}

/// osc9.zig:15. `room` is whether a NUL still fits the capture.
fn osc9(data: &[u8], room: bool) -> Option<Command<'_>> {
    if let Some(cmd) = conemu(data, room) {
        return cmd;
    }
    room.then_some(Command::ShowDesktopNotification { title: b"", body: data })
}

/// The ConEmu forms: `None` falls through to a notification, `Some(None)` is invalid.
fn conemu<'a>(data: &'a [u8], room: bool) -> Option<Option<Command<'a>>> {
    let with_nul = |c: Command<'a>| -> Option<Option<Command<'a>>> { Some(room.then_some(c)) };
    let value: &[u8] = data.get(2..).unwrap_or_default();
    let semi = data.get(1) == Some(&b';');
    match *data.first()? {
        b'1' => match *data.get(1)? {
            b';' => {
                let ms = zig_parse_unsigned(value, 10, u64::from(u16::MAX))
                    .map_or(100, |n| n.min(10_000));
                Some(Some(Command::ConemuSleep { duration_ms: ms as u16 }))
            }
            b'0' => {
                if data.len() == 2 {
                    return Some(Some(Command::ConemuXtermEmulation {
                        keyboard: Some(true),
                        output: Some(true),
                    }));
                }
                if data.len() < 4 || data[2] != b';' {
                    return None;
                }
                let (keyboard, output) = match data[3] {
                    b'0' => (Some(false), Some(false)),
                    b'1' => (Some(true), Some(true)),
                    b'2' => (None, Some(false)),
                    b'3' => (None, Some(true)),
                    _ => return None,
                };
                Some(Some(Command::ConemuXtermEmulation { keyboard, output }))
            }
            b'1' => {
                if data.len() < 3 || data[2] != b';' {
                    return None;
                }
                with_nul(Command::ConemuComment(&data[3..]))
            }
            b'2' => Some(Some(Command::SemanticPrompt(SemanticPrompt {
                action: PromptAction::FreshLineNewPrompt,
                options: b"",
            }))),
            _ => None,
        },
        b'2' if semi => with_nul(Command::ConemuShowMessageBox(value)),
        b'3' if semi => {
            if data.len() == 2 {
                return Some(Some(Command::ConemuChangeTabTitle(TabTitle::Reset)));
            }
            with_nul(Command::ConemuChangeTabTitle(TabTitle::Value(value)))
        }
        b'4' if semi => {
            let state = match *data.get(2)? {
                b'0' => ProgressState::Remove,
                b'1' => ProgressState::Set,
                b'2' => ProgressState::Error,
                b'3' => ProgressState::Indeterminate,
                b'4' => ProgressState::Pause,
                _ => return None,
            };
            let mut progress = (state == ProgressState::Set).then_some(0);
            if matches!(state, ProgressState::Set | ProgressState::Error | ProgressState::Pause)
                && data.get(3) == Some(&b';')
            {
                progress = zig_parse_unsigned(&data[4..], 10, u64::MAX).map(|n| n.min(100) as u8);
            }
            Some(Some(Command::ConemuProgressReport(ProgressReport { state, progress })))
        }
        b'5' => Some(Some(Command::ConemuWaitInput)),
        b'6' if semi => with_nul(Command::ConemuGuimacro(value)),
        b'7' if semi => with_nul(Command::ConemuRunProcess(value)),
        b'8' if semi => with_nul(Command::ConemuOutputEnvironmentVariable(value)),
        b'9' if semi => with_nul(Command::ReportPwd(value)),
        _ => None,
    }
}

/// iterm2.zig:66: `Key=value`, the key ASCII case-insensitive. Only `Copy=:b64` and
/// `CurrentDir=path` act; every other iTerm2 key, and any unknown one, is invalid.
fn iterm2(data: &[u8]) -> Option<Command<'_>> {
    let (key, value) = match find(data, b'=') {
        Some(i) => (&data[..i], Some(&data[i + 1..])),
        None => (data, None),
    };
    if !ITERM2_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)) {
        return None;
    }
    if key.eq_ignore_ascii_case(b"Copy") {
        let rest = value?.strip_prefix(b":")?;
        if rest.is_empty() || rest == b"?" {
            return None;
        }
        return Some(Command::ClipboardContents { kind: b'c', data: rest });
    }
    if key.eq_ignore_ascii_case(b"CurrentDir") {
        let v = value.filter(|v| !v.is_empty())?;
        return Some(Command::ReportPwd(v));
    }
    None
}

/// iterm2.zig:14 `Key`.
const ITERM2_KEYS: [&[u8]; 36] = [
    b"AddAnnotation",
    b"AddHiddenAnnotation",
    b"Block",
    b"Button",
    b"ClearCapturedOutput",
    b"ClearScrollback",
    b"Copy",
    b"CopyToClipboard",
    b"CurrentDir",
    b"CursorShape",
    b"Custom",
    b"Disinter",
    b"EndCopy",
    b"File",
    b"FileEnd",
    b"FilePart",
    b"HighlightCursorLine",
    b"MultipartFile",
    b"OpenURL",
    b"PopKeyLabels",
    b"PushKeyLabels",
    b"RemoteHost",
    b"ReportCellSize",
    b"ReportVariable",
    b"RequestAttention",
    b"RequestUpload",
    b"SetBackgroundImageFile",
    b"SetBadgeFormat",
    b"SetColors",
    b"SetKeyLabel",
    b"SetMark",
    b"SetProfile",
    b"SetUserVar",
    b"ShellIntegrationVersion",
    b"StealFocus",
    b"UnicodeVersion",
];

// ---------------------------------------------------------------------------------------------
// Colour operations (osc/parsers/color.zig)

/// Which colour OSC (color.zig `Operation`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Operation {
    /// OSC 4: palette (0–255) and special (256–260) colours.
    Palette,
    /// OSC 5: special colours 0–4.
    Special,
    /// OSC 104.
    ResetPalette,
    /// OSC 105.
    ResetSpecial,
    /// OSC 10–19, starting at this one.
    Dynamic(Dynamic),
    /// OSC 110–119.
    ResetDynamic(Dynamic),
}

/// What a colour request names (color.zig `Target`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    Palette(u8),
    Special(Special),
    Dynamic(Dynamic),
}

/// One colour request (color.zig `Request`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Request {
    Set {
        target: Target,
        color: Rgb,
    },
    Query(Target),
    Reset(Target),
    /// OSC 104 with no index.
    ResetPalette,
    /// OSC 105 with no index.
    ResetSpecial,
}

/// A colour OSC: its operation and data; [`ColorOperation::requests`] parses the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorOperation<'a> {
    pub op: Operation,
    data: &'a [u8],
    pub terminator: Terminator,
}

impl<'a> ColorOperation<'a> {
    pub fn requests(&self) -> Requests<'a> {
        let start = match self.op {
            Operation::Dynamic(d) => Some(d),
            _ => None,
        };
        Requests {
            op: self.op,
            tokens: self.data.split(semi as fn(&u8) -> bool),
            emitted: false,
            done: false,
            dynamic: start,
        }
    }
}

fn semi(b: &u8) -> bool {
    *b == b';'
}

/// The requests of a colour OSC, in order (color.zig:117 `parseColor`).
#[derive(Clone, Debug)]
pub struct Requests<'a> {
    op: Operation,
    tokens: core::slice::Split<'a, u8, fn(&u8) -> bool>,
    emitted: bool,
    done: bool,
    /// The dynamic colour the next OSC 10–19 value sets.
    dynamic: Option<Dynamic>,
}

/// OSC 4/104 index: 0–255 palette, 256–260 special.
fn osc4_target(n: u64) -> Option<Target> {
    match u8::try_from(n) {
        Ok(i) => Some(Target::Palette(i)),
        Err(_) => Special::from_index(n - 256).map(Target::Special),
    }
}

impl<'a> Requests<'a> {
    /// `tokenizeScalar`: the next non-empty `;` piece.
    fn token(&mut self) -> Option<&'a [u8]> {
        self.tokens.by_ref().find(|t| !t.is_empty())
    }

    fn target(&self, token: &[u8]) -> Option<Target> {
        let n = zig_parse_int(token, 0, 511)?.unsigned_abs();
        match self.op {
            Operation::Special | Operation::ResetSpecial => {
                Special::from_index(n).map(Target::Special)
            }
            _ => osc4_target(n),
        }
    }

    fn step(&mut self) -> Option<Request> {
        match self.op {
            Operation::Palette | Operation::Special => {
                let index = self.token()?;
                let spec = self.token()?;
                let target = self.target(index)?;
                if spec == b"?" {
                    return Some(Request::Query(target));
                }
                color::parse(spec).ok().map(|color| Request::Set { target, color })
            }
            Operation::ResetPalette | Operation::ResetSpecial => loop {
                let Some(t) = self.token() else {
                    if self.emitted {
                        return None;
                    }
                    return Some(if self.op == Operation::ResetPalette {
                        Request::ResetPalette
                    } else {
                        Request::ResetSpecial
                    });
                };
                // Invalid entries are skipped.
                if let Some(target) = self.target(t) {
                    return Some(Request::Reset(target));
                }
            },
            Operation::Dynamic(_) => {
                let d = self.dynamic?;
                let spec = self.token()?;
                let req = if spec == b"?" {
                    Request::Query(Target::Dynamic(d))
                } else {
                    Request::Set { target: Target::Dynamic(d), color: color::parse(spec).ok()? }
                };
                self.dynamic = d.next();
                Some(req)
            }
            Operation::ResetDynamic(d) => {
                // OSC 110–119 with any argument do nothing.
                if self.token().is_some() || self.emitted {
                    return None;
                }
                Some(Request::Reset(Target::Dynamic(d)))
            }
        }
    }
}

impl Iterator for Requests<'_> {
    type Item = Request;

    fn next(&mut self) -> Option<Request> {
        if self.done {
            return None;
        }
        let r = self.step();
        match r {
            Some(_) => self.emitted = true,
            None => self.done = true,
        }
        // A reset-all is the whole list.
        if matches!(r, Some(Request::ResetPalette | Request::ResetSpecial)) {
            self.done = true;
        }
        r
    }
}

// ---------------------------------------------------------------------------------------------
// OSC 21 (osc/parsers/kitty_color.zig, kitty/color.zig)

/// kitty's named colour keys (kitty/color.zig `Special`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KittySpecial {
    Foreground,
    Background,
    SelectionForeground,
    SelectionBackground,
    Cursor,
    CursorText,
    VisualBell,
    SecondTransparentBackground,
}

impl KittySpecial {
    pub const ALL: [KittySpecial; 8] = [
        KittySpecial::Foreground,
        KittySpecial::Background,
        KittySpecial::SelectionForeground,
        KittySpecial::SelectionBackground,
        KittySpecial::Cursor,
        KittySpecial::CursorText,
        KittySpecial::VisualBell,
        KittySpecial::SecondTransparentBackground,
    ];

    pub fn name(self) -> &'static [u8] {
        match self {
            KittySpecial::Foreground => b"foreground",
            KittySpecial::Background => b"background",
            KittySpecial::SelectionForeground => b"selection_foreground",
            KittySpecial::SelectionBackground => b"selection_background",
            KittySpecial::Cursor => b"cursor",
            KittySpecial::CursorText => b"cursor_text",
            KittySpecial::VisualBell => b"visual_bell",
            KittySpecial::SecondTransparentBackground => b"second_transparent_background",
        }
    }
}

/// An OSC 21 key (kitty/color.zig `Kind`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KittyKey {
    Palette(u8),
    Special(KittySpecial),
}

impl KittyKey {
    /// 255 + the 8 names: OSC 21 takes at most twice this many requests.
    pub const MAX: usize = 255 + 8;

    pub fn parse(key: &[u8]) -> Option<KittyKey> {
        if let Some(s) = KittySpecial::ALL.iter().find(|s| s.name() == key) {
            return Some(KittyKey::Special(*s));
        }
        zig_parse_unsigned(key, 10, 255).map(|n| KittyKey::Palette(n as u8))
    }

    /// Whether a query for it is answered even when the colour is unset (with an empty value).
    pub fn has_terminal_query_color(self) -> bool {
        matches!(
            self,
            KittyKey::Palette(_)
                | KittyKey::Special(
                    KittySpecial::Foreground | KittySpecial::Background | KittySpecial::Cursor
                )
        )
    }

    fn write(self, out: &mut impl FnMut(&[u8])) {
        match self {
            KittyKey::Palette(i) => out(Decimal::new(u64::from(i)).as_bytes()),
            KittyKey::Special(s) => out(s.name()),
        }
    }
}

/// One OSC 21 request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KittyRequest {
    Query(KittyKey),
    Set { key: KittyKey, color: Rgb },
    Reset(KittyKey),
}

/// An OSC 21 (kitty colour protocol); [`KittyColor::requests`] lists it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KittyColor<'a> {
    data: &'a [u8],
    pub terminator: Terminator,
}

impl<'a> KittyColor<'a> {
    /// `None` past ghostty's request limit (2 × [`KittyKey::MAX`]).
    fn new(data: &'a [u8], terminator: Terminator) -> Option<Self> {
        let mut n = 0;
        for kv in data.split(|&b| b == b';') {
            if n >= KittyKey::MAX * 2 {
                return None;
            }
            if kitty_request(kv).is_some() {
                n += 1;
            }
        }
        Some(Self { data, terminator })
    }

    pub fn requests(&self) -> impl Iterator<Item = KittyRequest> + 'a {
        self.data.split(|&b| b == b';').filter_map(kitty_request)
    }
}

/// kitty_color.zig:40: `key=value`, `key` reset, `key=?` query. Bad keys and specs are skipped.
fn kitty_request(kv: &[u8]) -> Option<KittyRequest> {
    let (k, rest) = match find(kv, b'=') {
        Some(i) => (&kv[..i], &kv[i + 1..]),
        None => (kv, &b""[..]),
    };
    if k.is_empty() {
        return None;
    }
    let key = KittyKey::parse(k)?;
    let value = color::trim(rest, b" ");
    Some(if value.is_empty() {
        KittyRequest::Reset(key)
    } else if value == b"?" {
        KittyRequest::Query(key)
    } else {
        KittyRequest::Set { key, color: color::parse(value).ok()? }
    })
}

// ---------------------------------------------------------------------------------------------
// Applying colour operations and replying (stream_terminal.zig:656, 777)

/// A small decimal, for replies.
struct Decimal {
    buf: [u8; 20],
    start: usize,
}

impl Decimal {
    fn new(mut n: u64) -> Self {
        let mut buf = [0u8; 20];
        let mut start = buf.len();
        loop {
            start -= 1;
            buf[start] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        Self { buf, start }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[self.start..]
    }
}

/// What a terminal answers for an xterm colour query (Terminal.zig:159 `colorForXterm`): the
/// cursor falls back to the foreground; special and the other dynamic colours have none.
pub fn color_for_xterm(colors: &Colors, target: Target) -> Option<Rgb> {
    match target {
        Target::Palette(i) => Some(colors.palette.current[usize::from(i)]),
        Target::Dynamic(Dynamic::Foreground) => colors.foreground.get(),
        Target::Dynamic(Dynamic::Background) => colors.background.get(),
        Target::Dynamic(Dynamic::Cursor) => colors.cursor.get().or(colors.foreground.get()),
        Target::Dynamic(_) | Target::Special(_) => None,
    }
}

/// What a terminal answers for an OSC 21 query (Terminal.zig:185 `colorForKitty`): no cursor
/// fallback.
pub fn color_for_kitty(colors: &Colors, key: KittyKey) -> Option<Rgb> {
    match key {
        KittyKey::Palette(i) => Some(colors.palette.current[usize::from(i)]),
        KittyKey::Special(KittySpecial::Foreground) => colors.foreground.get(),
        KittyKey::Special(KittySpecial::Background) => colors.background.get(),
        KittyKey::Special(KittySpecial::Cursor) => colors.cursor.get(),
        KittyKey::Special(_) => None,
    }
}

/// What a colour command changed, for the owner's dirty flags.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ColorEffect {
    /// A palette entry was set or reset (ghostty's `dirty.palette`).
    pub palette: bool,
}

/// Run a colour command (OSC 4, 5, 10–19, 104, 105, 110–119 and 21) against `colors`, in
/// request order, writing the query replies to `reply` (several calls make one reply; a query
/// sees the sets before it). Any other command does nothing. As ghostty's handler: special
/// colours and the dynamic colours past the cursor are parsed and ignored; OSC 104 with no
/// index resets the entries the palette's mask names.
pub fn run_colors(
    cmd: &Command<'_>,
    colors: &mut Colors,
    reply: &mut impl FnMut(&[u8]),
) -> ColorEffect {
    let mut fx = ColorEffect::default();
    match cmd {
        Command::ColorOperation(op) => {
            let term = op.terminator.bytes();
            for req in op.requests() {
                match req {
                    Request::Set { target, color } => match target {
                        Target::Palette(i) => {
                            fx.palette = true;
                            colors.palette.set(i, color);
                        }
                        Target::Dynamic(d) => {
                            if let Some(c) = dynamic_mut(colors, d) {
                                c.set(color);
                            }
                        }
                        Target::Special(_) => {}
                    },
                    Request::Reset(target) => match target {
                        Target::Palette(i) => {
                            fx.palette = true;
                            colors.palette.reset(i);
                        }
                        Target::Dynamic(d) => {
                            if let Some(c) = dynamic_mut(colors, d) {
                                c.reset();
                            }
                        }
                        Target::Special(_) => {}
                    },
                    Request::ResetPalette => {
                        if colors.palette.reset_modified() {
                            fx.palette = true;
                        }
                    }
                    Request::ResetSpecial => {}
                    Request::Query(target) => {
                        let Some(c) = color_for_xterm(colors, target) else { continue };
                        match target {
                            Target::Palette(i) => {
                                reply(b"\x1b]4;");
                                reply(Decimal::new(u64::from(i)).as_bytes());
                                reply(b";");
                            }
                            Target::Dynamic(d) => {
                                reply(b"\x1b]");
                                reply(Decimal::new(u64::from(d.code())).as_bytes());
                                reply(b";");
                            }
                            Target::Special(_) => continue,
                        }
                        reply(&color::rgb16(c));
                        reply(term);
                    }
                }
            }
        }
        Command::KittyColorProtocol(k) => {
            let mut wrote = false;
            for req in k.requests() {
                match req {
                    KittyRequest::Set { key, color } => match key {
                        KittyKey::Palette(i) => {
                            fx.palette = true;
                            colors.palette.set(i, color);
                        }
                        KittyKey::Special(s) => {
                            if let Some(c) = kitty_mut(colors, s) {
                                c.set(color);
                            }
                        }
                    },
                    KittyRequest::Reset(key) => match key {
                        KittyKey::Palette(i) => {
                            fx.palette = true;
                            colors.palette.reset(i);
                        }
                        KittyKey::Special(s) => {
                            if let Some(c) = kitty_mut(colors, s) {
                                c.reset();
                            }
                        }
                    },
                    KittyRequest::Query(key) => {
                        let c = color_for_kitty(colors, key);
                        if c.is_none() && !key.has_terminal_query_color() {
                            continue;
                        }
                        if !wrote {
                            reply(b"\x1b]21");
                            wrote = true;
                        }
                        reply(b";");
                        key.write(reply);
                        reply(b"=");
                        if let Some(c) = c {
                            reply(&color::rgb8(c));
                        }
                    }
                }
            }
            if wrote {
                reply(k.terminator.bytes());
            }
        }
        _ => {}
    }
    fx
}

fn dynamic_mut(colors: &mut Colors, d: Dynamic) -> Option<&mut color::DynamicRgb> {
    match d {
        Dynamic::Foreground => Some(&mut colors.foreground),
        Dynamic::Background => Some(&mut colors.background),
        Dynamic::Cursor => Some(&mut colors.cursor),
        _ => None,
    }
}

fn kitty_mut(colors: &mut Colors, s: KittySpecial) -> Option<&mut color::DynamicRgb> {
    match s {
        KittySpecial::Foreground => Some(&mut colors.foreground),
        KittySpecial::Background => Some(&mut colors.background),
        KittySpecial::Cursor => Some(&mut colors.cursor),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// OSC 22 (mouse.zig `Shape`)

/// A pointer shape (mouse.zig:8 `Shape`): the CSS cursor names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MouseShape {
    Default,
    ContextMenu,
    Help,
    Pointer,
    Progress,
    Wait,
    Cell,
    Crosshair,
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    AllScroll,
    ColResize,
    RowResize,
    NResize,
    EResize,
    SResize,
    WResize,
    NeResize,
    NwResize,
    SeResize,
    SwResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ZoomIn,
    ZoomOut,
}

impl MouseShape {
    /// A CSS name or one of the xterm and foot aliases ghostty takes (mouse.zig:101),
    /// case-sensitive. An unknown name is `None` (ignored).
    pub fn from_name(name: &[u8]) -> Option<MouseShape> {
        use MouseShape as M;
        Some(match name {
            b"default" | b"left_ptr" => M::Default,
            b"context-menu" => M::ContextMenu,
            b"help" | b"question_arrow" => M::Help,
            b"pointer" | b"hand" => M::Pointer,
            b"progress" | b"left_ptr_watch" => M::Progress,
            b"wait" | b"watch" => M::Wait,
            b"cell" => M::Cell,
            b"crosshair" | b"cross" => M::Crosshair,
            b"text" | b"xterm" => M::Text,
            b"vertical-text" => M::VerticalText,
            b"alias" | b"dnd-link" => M::Alias,
            b"copy" | b"dnd-copy" => M::Copy,
            b"move" | b"dnd-move" => M::Move,
            b"no-drop" | b"dnd-no-drop" => M::NoDrop,
            b"not-allowed" | b"crossed_circle" => M::NotAllowed,
            b"grab" | b"hand1" => M::Grab,
            b"grabbing" => M::Grabbing,
            b"all-scroll" | b"fleur" => M::AllScroll,
            b"col-resize" => M::ColResize,
            b"row-resize" => M::RowResize,
            b"n-resize" | b"top_side" => M::NResize,
            b"e-resize" | b"right_side" => M::EResize,
            b"s-resize" | b"bottom_side" => M::SResize,
            b"w-resize" | b"left_side" => M::WResize,
            b"ne-resize" | b"top_right_corner" => M::NeResize,
            b"nw-resize" | b"top_left_corner" => M::NwResize,
            b"se-resize" | b"bottom_right_corner" => M::SeResize,
            b"sw-resize" | b"bottom_left_corner" => M::SwResize,
            b"ew-resize" => M::EwResize,
            b"ns-resize" => M::NsResize,
            b"nesw-resize" => M::NeswResize,
            b"nwse-resize" => M::NwseResize,
            b"zoom-in" => M::ZoomIn,
            b"zoom-out" => M::ZoomOut,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// OSC 133 (osc/parsers/semantic_prompt.zig)

/// What an OSC 133 marks (semantic_prompt.zig `Action`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PromptAction {
    /// L
    FreshLine,
    /// A
    FreshLineNewPrompt,
    /// N
    NewCommand,
    /// P
    PromptStart,
    /// B
    EndPromptStartInput,
    /// I
    EndPromptStartInputTerminateEol,
    /// C
    EndInputStartOutput,
    /// D
    EndCommand,
}

/// `cl=` (semantic_prompt.zig `Click`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Click {
    Line,
    Multiple,
    ConservativeVertical,
    SmartVertical,
}

/// `k=` (semantic_prompt.zig `PromptKind`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PromptKind {
    Initial,
    Right,
    Continuation,
    Secondary,
}

/// `redraw=` (semantic_prompt.zig `Redraw`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Redraw {
    True,
    False,
    Last,
}

/// `click_events=`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClickEvents {
    Absolute,
    Relative,
}

/// A printf `%q` or percent-encoded command line that does not decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeError;

/// An OSC 133 mark and its options, read on demand (semantic_prompt.zig:16 `Command`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SemanticPrompt<'a> {
    pub action: PromptAction,
    /// The `;`-separated options, unvalidated.
    pub options: &'a [u8],
}

impl<'a> SemanticPrompt<'a> {
    /// semantic_prompt.zig:315.
    fn parse(data: &'a [u8]) -> Option<Self> {
        use PromptAction as P;
        let action = match *data.first()? {
            b'A' => P::FreshLineNewPrompt,
            b'B' => P::EndPromptStartInput,
            b'I' => P::EndPromptStartInputTerminateEol,
            b'C' => P::EndInputStartOutput,
            b'D' => P::EndCommand,
            b'N' => P::NewCommand,
            b'P' => P::PromptStart,
            b'L' if data.len() == 1 => return Some(Self { action: P::FreshLine, options: b"" }),
            _ => return None,
        };
        match data {
            [_] => Some(Self { action, options: b"" }),
            [_, b';', rest @ ..] => Some(Self { action, options: rest }),
            _ => None,
        }
    }

    /// The raw value of the first option named `key` (semantic_prompt.zig:147 `Option.read`).
    pub fn option(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.options.split(|&b| b == b';').find_map(|full| {
            let eq = find(full, b'=')?;
            (&full[..eq] == key).then(|| &full[eq + 1..])
        })
    }

    pub fn aid(&self) -> Option<&'a [u8]> {
        self.option(b"aid")
    }

    pub fn cl(&self) -> Option<Click> {
        match self.option(b"cl")? {
            b"m" => Some(Click::Multiple),
            b"v" => Some(Click::ConservativeVertical),
            b"w" => Some(Click::SmartVertical),
            b"line" => Some(Click::Line),
            _ => None,
        }
    }

    pub fn prompt_kind(&self) -> Option<PromptKind> {
        match self.option(b"k")? {
            b"i" => Some(PromptKind::Initial),
            b"r" => Some(PromptKind::Right),
            b"c" => Some(PromptKind::Continuation),
            b"s" => Some(PromptKind::Secondary),
            _ => None,
        }
    }

    pub fn err(&self) -> Option<&'a [u8]> {
        self.option(b"err")
    }

    pub fn redraw(&self) -> Option<Redraw> {
        match self.option(b"redraw")? {
            b"0" => Some(Redraw::False),
            b"1" => Some(Redraw::True),
            b"last" => Some(Redraw::Last),
            _ => None,
        }
    }

    pub fn special_key(&self) -> Option<bool> {
        match self.option(b"special_key")? {
            b"0" => Some(false),
            b"1" => Some(true),
            _ => None,
        }
    }

    pub fn click_events(&self) -> Option<ClickEvents> {
        match self.option(b"click_events")? {
            b"1" => Some(ClickEvents::Absolute),
            b"2" => Some(ClickEvents::Relative),
            _ => None,
        }
    }

    pub fn cmdline(&self) -> Option<&'a [u8]> {
        self.option(b"cmdline")
    }

    pub fn cmdline_url(&self) -> Option<&'a [u8]> {
        self.option(b"cmdline_url")
    }

    /// OSC 133;D's exit code: the first option as an `i32`.
    pub fn exit_code(&self) -> Option<i32> {
        if self.options.is_empty() {
            return None;
        }
        let first = self.options.split(|&b| b == b';').next()?;
        zig_parse_int(first, i64::from(i32::MIN), i64::from(i32::MAX)).map(|n| n as i32)
    }

    /// The command line, decoded (semantic_prompt.zig:50 `writeCommandLine`): `cmdline` as
    /// printf `%q`, else `cmdline_url` percent-encoded, else nothing. On an error some bytes
    /// may have been written.
    pub fn write_command_line(&self, out: &mut impl FnMut(u8)) -> Result<(), DecodeError> {
        if let Some(c) = self.cmdline() {
            return printf_q_decode(c, out);
        }
        if let Some(c) = self.cmdline_url() {
            return url_percent_decode(c, out);
        }
        Ok(())
    }
}

/// Undo bash's `printf %q` (os/string_encoding.zig:6 `printfQDecode`).
pub fn printf_q_decode(buf: &[u8], out: &mut impl FnMut(u8)) -> Result<(), DecodeError> {
    let data = if buf.starts_with(b"$'") {
        if buf.len() < 3 || !buf.ends_with(b"'") {
            return Err(DecodeError);
        }
        &buf[2..buf.len() - 1]
    } else if buf.starts_with(b"'") {
        if buf.len() < 2 || !buf.ends_with(b"'") {
            return Err(DecodeError);
        }
        &buf[1..buf.len() - 1]
    } else {
        buf
    };
    let mut src = 0;
    while src < data.len() {
        if data[src] != b'\\' {
            out(data[src]);
            src += 1;
            continue;
        }
        let c = *data.get(src + 1).ok_or(DecodeError)?;
        out(match c {
            b' ' | b'\\' | b'"' | b'\'' | b'$' => c,
            b'e' => 0x1b,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            _ => return Err(DecodeError),
        });
        src += 2;
    }
    Ok(())
}

/// Undo URL percent-encoding (os/string_encoding.zig:164 `urlPercentDecode`).
pub fn url_percent_decode(buf: &[u8], out: &mut impl FnMut(u8)) -> Result<(), DecodeError> {
    let mut src = 0;
    while src < buf.len() {
        if buf[src] != b'%' {
            out(buf[src]);
            src += 1;
            continue;
        }
        if src + 2 >= buf.len() {
            return Err(DecodeError);
        }
        let hex = |c: u8| char::from(c).to_digit(16).ok_or(DecodeError);
        let (hi, lo) = (hex(buf[src + 1])?, hex(buf[src + 2])?);
        out((hi << 4 | lo) as u8);
        src += 3;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// OSC 66 (osc/parsers/kitty_text_sizing.zig)

/// The longest OSC 66 text.
pub const MAX_TEXT_SIZING_PAYLOAD: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum VAlign {
    Top,
    Bottom,
    Center,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HAlign {
    Left,
    Right,
    Center,
}

/// OSC 66 (kitty_text_sizing.zig:28 `OSC`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextSizing<'a> {
    /// 1–7.
    pub scale: u8,
    /// 0–7, 0 the default.
    pub width: u8,
    /// 0–15.
    pub numerator: u8,
    /// 0–15.
    pub denominator: u8,
    pub valign: VAlign,
    pub halign: HAlign,
    pub text: &'a [u8],
}

impl<'a> TextSizing<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        let at = find(data, b';')?;
        let text = &data[at + 1..];
        if text.len() > MAX_TEXT_SIZING_PAYLOAD || !is_safe_utf8(text) {
            return None;
        }
        let mut cmd = Self {
            scale: 1,
            width: 0,
            numerator: 0,
            denominator: 0,
            valign: VAlign::Top,
            halign: HAlign::Left,
            text,
        };
        if at > 0 {
            for kv in data[..at].split(|&b| b == b':') {
                let mut it = kv.split(|&b| b == b'=');
                let k = it.next().unwrap_or_default();
                let (Some(value), [k]) = (it.next(), k) else { continue };
                cmd.update(*k, value);
            }
        }
        Some(cmd)
    }

    /// One `k=v`; an unknown key or a bad value is skipped.
    fn update(&mut self, key: u8, value: &[u8]) {
        let Some(v) = zig_parse_int(value, 0, 15).map(|v| v as u8) else { return };
        match key {
            b's' if (1..=7).contains(&v) => self.scale = v,
            b'w' if v <= 7 => self.width = v,
            b'n' => self.numerator = v,
            b'd' => self.denominator = v,
            b'v' => {
                if let Some(a) = [VAlign::Top, VAlign::Bottom, VAlign::Center].get(usize::from(v)) {
                    self.valign = *a;
                }
            }
            b'h' => {
                if let Some(a) = [HAlign::Left, HAlign::Right, HAlign::Center].get(usize::from(v)) {
                    self.halign = *a;
                }
            }
            _ => {}
        }
    }
}

/// kitty's "escape code safe UTF-8" (osc/encoding.zig:11 `isSafeUtf8`): valid UTF-8 with no C0,
/// DEL or C1 code point.
pub fn is_safe_utf8(s: &[u8]) -> bool {
    core::str::from_utf8(s).is_ok_and(|s| {
        s.chars().all(|c| !matches!(c, '\0'..='\x1f' | '\x7f' | '\u{80}'..='\u{9f}'))
    })
}

// ---------------------------------------------------------------------------------------------
// OSC 72 and 5522 (kitty_dnd_protocol.zig, kitty_clipboard_protocol.zig)

/// Zig's `std.ascii.whitespace`: space, tab, LF, CR, VT, FF.
fn is_zig_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The value of `name=` in `:`-separated kitty metadata (both protocols' `Option.read`).
fn kitty_option<'a>(metadata: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut pos = 0;
    while pos < metadata.len() {
        while pos < metadata.len() && is_zig_space(metadata[pos]) {
            pos += 1;
        }
        if pos >= metadata.len() {
            return None;
        }
        if !metadata[pos..].starts_with(name) {
            pos += find(&metadata[pos..], b':')? + 1;
            continue;
        }
        pos += name.len();
        while pos < metadata.len() && is_zig_space(metadata[pos]) {
            pos += 1;
        }
        if metadata.get(pos) != Some(&b'=') {
            return None;
        }
        let end = find(&metadata[pos..], b':').map_or(metadata.len(), |e| e + pos);
        return Some(trim_zig_space(&metadata[pos + 1..end]));
    }
    None
}

fn trim_zig_space(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| !is_zig_space(c)).unwrap_or(s.len());
    let end = s.iter().rposition(|&c| !is_zig_space(c)).map_or(start, |e| e + 1);
    &s[start..end]
}

/// OSC 72 `t=` (kitty_dnd_protocol.zig:28 `EventType`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DndEvent {
    AcceptDrops,
    StopAcceptingDrops,
    DropMove,
    DropDropped,
    RequestData,
    RequestError,
    OfferDrag,
    PresentData,
    ChangeDragImage,
    DragOfferEvent,
    DragOfferError,
    UriListData,
    Query,
}

/// OSC 72's integer keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DndInt {
    M,
    I,
    O,
    X,
    Y,
    /// `X`
    BigX,
    /// `Y`
    BigY,
}

impl DndInt {
    fn name(self) -> &'static [u8] {
        match self {
            DndInt::M => b"m",
            DndInt::I => b"i",
            DndInt::O => b"o",
            DndInt::X => b"x",
            DndInt::Y => b"y",
            DndInt::BigX => b"X",
            DndInt::BigY => b"Y",
        }
    }
}

/// OSC 72, kitty drag and drop (kitty_dnd_protocol.zig:14 `OSC`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KittyDnd<'a> {
    pub metadata: &'a [u8],
    pub payload: Option<&'a [u8]>,
    pub terminator: Terminator,
}

impl<'a> KittyDnd<'a> {
    pub fn event(&self) -> Option<DndEvent> {
        use DndEvent as E;
        Some(match kitty_option(self.metadata, b"t")? {
            b"a" => E::AcceptDrops,
            b"A" => E::StopAcceptingDrops,
            b"m" => E::DropMove,
            b"M" => E::DropDropped,
            b"r" => E::RequestData,
            b"R" => E::RequestError,
            b"o" => E::OfferDrag,
            b"p" => E::PresentData,
            b"P" => E::ChangeDragImage,
            b"e" => E::DragOfferEvent,
            b"E" => E::DragOfferError,
            b"k" => E::UriListData,
            b"q" => E::Query,
            _ => return None,
        })
    }

    pub fn int(&self, key: DndInt) -> Option<i32> {
        let v = kitty_option(self.metadata, key.name())?;
        zig_parse_int(v, i64::from(i32::MIN), i64::from(i32::MAX)).map(|n| n as i32)
    }
}

/// OSC 5522 `loc=`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClipboardLocation {
    Primary,
}

/// OSC 5522 `type=`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClipboardOperation {
    Read,
    Walias,
    Wdata,
    Write,
}

/// OSC 5522 `status=`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClipboardStatus {
    Data,
    Done,
    Ebusy,
    Einval,
    Eio,
    Enosys,
    Eperm,
    Ok,
}

/// OSC 5522's string keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClipboardText {
    Mime,
    Name,
    Password,
    Pw,
}

/// OSC 5522, kitty's clipboard protocol (kitty_clipboard_protocol.zig:17 `OSC`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KittyClipboard<'a> {
    pub metadata: &'a [u8],
    pub payload: Option<&'a [u8]>,
    pub terminator: Terminator,
}

impl<'a> KittyClipboard<'a> {
    /// `id=`, when it is a valid identifier (`[a-zA-Z0-9_+.-]+`).
    pub fn id(&self) -> Option<&'a [u8]> {
        kitty_option(self.metadata, b"id").filter(|v| {
            !v.is_empty() && v.iter().all(|&c| c.is_ascii_alphanumeric() || b"-_+.".contains(&c))
        })
    }

    pub fn loc(&self) -> Option<ClipboardLocation> {
        (kitty_option(self.metadata, b"loc")? == b"primary").then_some(ClipboardLocation::Primary)
    }

    pub fn text(&self, key: ClipboardText) -> Option<&'a [u8]> {
        let name: &[u8] = match key {
            ClipboardText::Mime => b"mime",
            ClipboardText::Name => b"name",
            ClipboardText::Password => b"password",
            ClipboardText::Pw => b"pw",
        };
        kitty_option(self.metadata, name)
    }

    pub fn status(&self) -> Option<ClipboardStatus> {
        use ClipboardStatus as S;
        Some(match kitty_option(self.metadata, b"status")? {
            b"DATA" => S::Data,
            b"DONE" => S::Done,
            b"EBUSY" => S::Ebusy,
            b"EINVAL" => S::Einval,
            b"EIO" => S::Eio,
            b"ENOSYS" => S::Enosys,
            b"EPERM" => S::Eperm,
            b"OK" => S::Ok,
            _ => return None,
        })
    }

    pub fn operation(&self) -> Option<ClipboardOperation> {
        use ClipboardOperation as O;
        Some(match kitty_option(self.metadata, b"type")? {
            b"read" => O::Read,
            b"walias" => O::Walias,
            b"wdata" => O::Wdata,
            b"write" => O::Write,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// OSC 3008 (osc/parsers/context_signal.zig)

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContextAction {
    Start,
    End,
}

/// `type=` (context_signal.zig:47 `ContextType`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContextType {
    Boot,
    Container,
    Vm,
    Elevate,
    Chpriv,
    Subcontext,
    Remote,
    Shell,
    Command,
    App,
    Service,
    Session,
}

/// `exit=` (context_signal.zig:67 `ExitStatus`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ExitStatus {
    Success,
    Failure,
    Crash,
    Interrupt,
}

/// The string fields (context_signal.zig:80 `Field`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContextText {
    User,
    Hostname,
    Machineid,
    Bootid,
    Comm,
    Cwd,
    Cmdline,
    Vm,
    Container,
    Targetuser,
    Targethost,
    Sessionid,
    Signal,
}

/// The integer fields.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContextInt {
    Pid,
    Pidfdid,
    Status,
}

/// OSC 3008 (context_signal.zig:18 `Command`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ContextSignal<'a> {
    pub action: ContextAction,
    /// 1–64 bytes, each 0x20–0x7E.
    pub id: &'a [u8],
    /// The `;`-separated `key=value` fields after the id.
    pub metadata: &'a [u8],
}

impl<'a> ContextSignal<'a> {
    /// context_signal.zig:203.
    fn parse(data: &'a [u8]) -> Option<Self> {
        let (action, rest) = if let Some(r) = data.strip_prefix(b"start=") {
            (ContextAction::Start, r)
        } else {
            (ContextAction::End, data.strip_prefix(b"end=")?)
        };
        let id_end = find(rest, b';').unwrap_or(rest.len());
        let id = &rest[..id_end];
        if id.is_empty() || id.len() > 64 || id.iter().any(|&c| !(0x20..=0x7e).contains(&c)) {
            return None;
        }
        let metadata = rest.get(id_end + 1..).unwrap_or_default();
        Some(Self { action, id, metadata })
    }

    /// The raw value of the first field named `key`.
    fn field(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.metadata.split(|&b| b == b';').find_map(|full| {
            let eq = find(full, b'=')?;
            (&full[..eq] == key).then(|| &full[eq + 1..])
        })
    }

    pub fn context_type(&self) -> Option<ContextType> {
        use ContextType as T;
        Some(match self.field(b"type")? {
            b"boot" => T::Boot,
            b"container" => T::Container,
            b"vm" => T::Vm,
            b"elevate" => T::Elevate,
            b"chpriv" => T::Chpriv,
            b"subcontext" => T::Subcontext,
            b"remote" => T::Remote,
            b"shell" => T::Shell,
            b"command" => T::Command,
            b"app" => T::App,
            b"service" => T::Service,
            b"session" => T::Session,
            _ => return None,
        })
    }

    pub fn exit(&self) -> Option<ExitStatus> {
        use ExitStatus as E;
        Some(match self.field(b"exit")? {
            b"success" => E::Success,
            b"failure" => E::Failure,
            b"crash" => E::Crash,
            b"interrupt" => E::Interrupt,
            _ => return None,
        })
    }

    pub fn int(&self, key: ContextInt) -> Option<u64> {
        let name: &[u8] = match key {
            ContextInt::Pid => b"pid",
            ContextInt::Pidfdid => b"pidfdid",
            ContextInt::Status => b"status",
        };
        let v = self.field(name)?;
        if !v.iter().all(u8::is_ascii_digit) {
            return None;
        }
        zig_parse_unsigned(v, 10, u64::MAX)
    }

    /// A string field; empty is `None`.
    pub fn text(&self, key: ContextText) -> Option<&'a [u8]> {
        use ContextText as C;
        let name: &[u8] = match key {
            C::User => b"user",
            C::Hostname => b"hostname",
            C::Machineid => b"machineid",
            C::Bootid => b"bootid",
            C::Comm => b"comm",
            C::Cwd => b"cwd",
            C::Cmdline => b"cmdline",
            C::Vm => b"vm",
            C::Container => b"container",
            C::Targetuser => b"targetuser",
            C::Targethost => b"targethost",
            C::Sessionid => b"sessionid",
            C::Signal => b"signal",
        };
        self.field(name).filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests;
