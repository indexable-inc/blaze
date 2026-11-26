//! The terminal: two screens, the modes, the scrolling region, tab stops and the state of a
//! session, driven by the parser as its [`Sink`](crate::parser::Sink). Ports ghostty's
//! Terminal.zig and the dispatch of stream.zig / stream_terminal.zig (vt-inventory.md §2–5,
//! §11, §12).
//!
//! Sans-IO: [`Terminal::feed`] takes bytes and returns nothing. What the program asked to have
//! written back (device attributes, cursor reports, mode reports, colour and kitty replies) is
//! bytes in a reply buffer ([`Terminal::take_replies`]); what the host is asked to do (ring the
//! bell, retitle the window, write the clipboard, read a kitty image file) is an [`Effect`] list
//! ([`Terminal::take_effects`]), whose text is [`Span`]s of the list's byte buffer. Both are
//! capped by [`Caps`]: past a cap a reply or effect is refused and counted in [`Stats`].

mod dispatch;
pub mod kitty;
pub mod modes;
mod ops;
mod osc;
mod print;

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::color::Colors;
use crate::dcs::tmux::Notification;
use crate::dcs::{DcsHandler, DcsOpts};
use crate::kitty::apc::{Apc, ApcOpts};
use crate::kitty::exec::{Answer, GraphicsOpts, KittyGraphics, Request, ScreenKey, UnaskedAnswer};
use crate::osc::{Clipboard, MouseShape, ProgressReport};
use crate::parser::Parser;
use crate::plant::VtPlant;
use crate::screen::{Screen, ScreenStats};

pub use kitty::{KittyCells, KittyScreen};
pub use modes::{Mode, ModeState, MouseEvents, MouseFormat};

/// Bytes of an [`Effects`] list's buffer ([`Effects::bytes`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

/// What the host is asked to do. Text is a [`Span`] of the [`Effects`] the effect came in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Effect {
    /// BEL.
    Bell,
    /// The title changed: read it with [`Terminal::title`].
    TitleChanged,
    /// Mode 2026 set (`true`) or reset: hold or release frames.
    SynchronizedOutput(bool),
    /// DECCOLM resized the terminal to this many columns; the window may follow.
    ColumnMode { cols: u16 },
    /// Mode 40 changed: with it reset, the host puts the size back to its window's (SH).
    EnableMode3(bool),
    /// Mode 5 (reverse colors) changed: everything needs drawing.
    ReverseColors(bool),
    /// OSC 7 set a local working directory, or cleared it: read it with [`Terminal::pwd`]
    /// (termio's `pwd_change`).
    PwdChanged,
    /// OSC 52 `?`: the program asks for the clipboard. Answer with
    /// [`Terminal::clipboard_reply`] (termio's `clipboard_read`; the host decides whether to).
    ClipboardRead(Clipboard),
    /// OSC 52 with data: write these bytes (base64-decoded) to the clipboard; empty clears it.
    ClipboardWrite { clipboard: Clipboard, contents: Span },
    /// OSC 9 and OSC 777 `notify`.
    DesktopNotification { title: Span, body: Span },
    /// OSC 9;4.
    Progress(ProgressReport),
    /// OSC 22: the pointer's shape, also kept ([`Terminal::mouse_shape`]).
    MouseShape(MouseShape),
    /// OSC 4/104 or OSC 21 set or reset a palette entry ([`Terminal::colors`]): cells drawn in
    /// palette colours need drawing (ghostty's `dirty.palette`).
    PaletteChanged,
    /// OSC 133;C: the command line is entered and its output starts (termio's `start_command`).
    CommandStarted,
    /// OSC 133;D: the command ended with this code (termio: the option as a byte, 1 when it
    /// does not fit one, 0 when absent).
    CommandFinished { exit_code: u8 },
    /// A tmux control-mode notification (`DCS 1000 p`, [`Opts::tmux_control_mode`]).
    Tmux(Notification<Span>),
    /// A kitty graphics load waits on host IO: [`Terminal::kitty_pending`] says what,
    /// [`Terminal::kitty_answer`] gives it.
    KittyIo,
}

/// The effects taken ([`Terminal::take_effects`]): the list and the bytes its spans name.
pub struct Effects<'a> {
    list: Vec<Effect, Alloc<'a>>,
    bytes: Vec<u8, Alloc<'a>>,
}

impl<'a> Effects<'a> {
    fn new_in(alloc: Alloc<'a>) -> Effects<'a> {
        Effects { list: Vec::new_in(alloc), bytes: Vec::new_in(alloc) }
    }

    pub fn as_slice(&self) -> &[Effect] {
        &self.list
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Effect> {
        self.list.iter()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The bytes a span of one of these effects names.
    pub fn bytes(&self, span: Span) -> &[u8] {
        let start = span.start as usize;
        self.bytes.get(start..start + span.len as usize).unwrap_or_default()
    }

    /// Copies `bytes` into the buffer, if they fit under `cap`.
    fn stash(&mut self, bytes: &[u8], cap: usize) -> Option<Span> {
        let start = self.bytes.len();
        if start + bytes.len() > cap || self.bytes.try_reserve(bytes.len()).is_err() {
            return None;
        }
        self.bytes.extend_from_slice(bytes);
        Some(Span { start: start as u32, len: bytes.len() as u32 })
    }

    /// Queues `e`, if the list is under `cap`.
    fn push(&mut self, e: Effect, cap: usize) -> bool {
        if self.list.len() >= cap || self.list.try_reserve(1).is_err() {
            return false;
        }
        self.list.push(e);
        true
    }

    /// Queues a tmux notification with its text copied in, whole or not at all.
    fn tmux(&mut self, n: Notification<&[u8]>, caps: &Caps) -> bool {
        let mark = self.bytes.len();
        let mut fits = true;
        let n = n.map(|b| {
            self.stash(b, caps.effect_bytes).unwrap_or_else(|| {
                fits = false;
                Span::default()
            })
        });
        if fits && self.push(Effect::Tmux(n), caps.effects) {
            return true;
        }
        self.bytes.truncate(mark);
        false
    }
}

/// A cell's pixel size, for the size reports (CSI 14/16 t, mode 2048): the mouse encoder's
/// [`Pixels`].
pub use crate::input::mouse::Pixels;
/// The host's color scheme (CSI ? 996 n): the reply encoder's.
pub use crate::reply::ColorScheme;

/// What the device attribute replies say (ghostty's `device_attributes.zig`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceAttributes<'a> {
    pub primary: crate::reply::Da1<'a>,
    pub secondary: crate::reply::Da2,
    /// DA3's unit id; `None`: no reply (the app's choice).
    pub tertiary: Option<u32>,
}

impl DeviceAttributes<'static> {
    /// libghostty's defaults: `\x1b[?62;22c`, `\x1b[>1;0;0c`, `\x1bP!|00000000\x1b\\`.
    pub const LIB: DeviceAttributes<'static> = DeviceAttributes {
        primary: crate::reply::Da1::LIB,
        secondary: crate::reply::Da2::LIB,
        tertiary: Some(0),
    };
}

/// Byte and entry caps on what the terminal keeps (AGENTS.md: every table has a cap).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    /// Reply bytes not yet taken.
    pub replies: usize,
    /// Effects not yet taken.
    pub effects: usize,
    /// The bytes those effects carry (clipboard contents, notification text, tmux output).
    pub effect_bytes: usize,
    /// An OSC's bytes. Only OSC 52, 66, 72 and 5522 come near it: ghostty bounds the others at
    /// [`crate::osc::MAX_BUF`], and so does the capture here.
    pub osc: usize,
    /// A DCS request's bytes (an XTGETTCAP payload, a tmux line or block; ghostty: 1 MiB).
    pub dcs: usize,
    /// The title's bytes (ghostty truncates at 1024, stream_terminal.zig `windowTitle`).
    pub title: usize,
    /// Titles CSI 22 t can push.
    pub title_stack: usize,
    /// OSC 7's URL (ghostty truncates at 4096, stream_terminal.zig `reportPwd`).
    pub pwd: usize,
}

impl Caps {
    pub const DEFAULT: Caps = Caps {
        replies: 64 << 10,
        effects: 1024,
        effect_bytes: 8 << 20,
        osc: 8 << 20,
        dcs: DcsOpts::MAX_BYTES,
        title: 1024,
        title_stack: 10,
        pwd: 4096,
    };
}

/// How to make a terminal: the size and scrollback are the caller's choice; the rest defaults.
#[derive(Clone, Copy, Debug)]
pub struct Opts<'a> {
    pub cols: u16,
    pub rows: u16,
    /// The primary screen's scrollback cap in bytes of pages (ghostty's `max_scrollback`).
    pub scrollback_bytes: usize,
    /// Rows in a page.
    pub page_rows: usize,
    /// Mode 2027's default (ghostty's `grapheme-width-method = unicode`).
    pub grapheme_cluster: bool,
    /// Cell pixel size; `None`: the pixel size reports get no reply.
    pub cell_px: Option<Pixels>,
    pub device: DeviceAttributes<'a>,
    /// XTVERSION's name; empty: `libghostty`, as ghostty's library says.
    pub xtversion: &'a [u8],
    /// The reply to ENQ; empty: none.
    pub enquiry: &'a [u8],
    /// This machine's name (`gethostname`): an OSC 7 URL must name it or `localhost`
    /// (termio's `isLocal`); empty: only `localhost`.
    pub hostname: &'a [u8],
    /// Whether `DCS 1000 p` enters tmux control mode ([`Effect::Tmux`]); ghostty's app has it.
    pub tmux_control_mode: bool,
    /// The APC protocols (kitty graphics, glyphs) and their byte caps.
    pub apc: ApcOpts,
    /// Kitty graphics storage: each screen's byte cap (0 disables), media, PNG decoder.
    pub graphics: GraphicsOpts,
    pub caps: Caps,
}

impl<'a> Opts<'a> {
    pub const fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> Opts<'a> {
        Opts {
            cols,
            rows,
            scrollback_bytes,
            page_rows: 256,
            grapheme_cluster: false,
            cell_px: None,
            device: DeviceAttributes::LIB,
            xtversion: b"",
            enquiry: b"",
            hostname: b"",
            tmux_control_mode: false,
            apc: ApcOpts::DEFAULT,
            graphics: GraphicsOpts::DEFAULT,
            caps: Caps::DEFAULT,
        }
    }
}

/// What the terminal refused or did not know since it was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Sequences ghostty logs as unsupported or invalid and ignores.
    pub unsupported: u64,
    /// Replies dropped: the buffer was at its cap, or the allocator refused.
    pub replies_dropped: u64,
    /// Effects dropped at the cap.
    pub effects_dropped: u64,
    /// OSC, DCS, APC strings cut at their caps (and not dispatched).
    pub strings_truncated: u64,
    /// Titles pushed past the stack's cap (the oldest is dropped).
    pub titles_evicted: u64,
}

/// The status line DECSASD selects; printing to it is discarded (S:2232).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StatusDisplay {
    #[default]
    Main,
    StatusLine,
}

/// The scrolling region, inclusive, 0-based.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

/// Which screen is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    Primary,
    Alternate,
}

/// Terminal flags that are not modes (ghostty's `Terminal.flags`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flags {
    pub mouse_event: MouseEvents,
    pub mouse_format: MouseFormat,
    /// XTMODKEYS `4;2`.
    pub modify_other_keys_2: bool,
    /// XTSHIFTESCAPE; `None` until a program says.
    pub mouse_shift_capture: Option<bool>,
    /// Clear the prompt on resize so the shell redraws it (OSC 133 `redraw`, default on).
    pub shell_redraws_prompt: crate::screen::PromptRedraw,
}

impl Flags {
    const DEFAULT: Flags = Flags {
        mouse_event: MouseEvents::None,
        mouse_format: MouseFormat::X10,
        modify_other_keys_2: false,
        mouse_shift_capture: None,
        shell_redraws_prompt: crate::screen::PromptRedraw::True,
    };
}

/// An OSC being gathered, capped.
struct StringBuf<'a> {
    bytes: Vec<u8, Alloc<'a>>,
    cap: usize,
    /// Cut at the cap (or the allocator refused): not dispatched.
    overflow: bool,
}

impl<'a> StringBuf<'a> {
    fn new(cap: usize, alloc: Alloc<'a>) -> StringBuf<'a> {
        StringBuf { bytes: Vec::new_in(alloc), cap, overflow: false }
    }

    fn start(&mut self) {
        self.bytes.clear();
        self.overflow = false;
    }

    fn put(&mut self, data: &[u8]) {
        if self.overflow {
            return;
        }
        if self.bytes.len() + data.len() > self.cap || self.bytes.try_reserve(data.len()).is_err() {
            self.overflow = true;
            return;
        }
        self.bytes.extend_from_slice(data);
    }
}

/// A terminal (ghostty's `Terminal` with its stream handler).
pub struct Terminal<'a, P: Planted<VtPlant> = NoPlant> {
    alloc: Alloc<'a>,
    opts: Opts<'a>,
    parser: Parser,
    primary: Screen<'a, P>,
    alternate: Option<Screen<'a, P>>,
    active: Which,
    cols: usize,
    rows: usize,
    region: Region,
    tabstops: Vec<bool, Alloc<'a>>,
    modes: ModeState,
    flags: Flags,
    previous_char: Option<u32>,
    status_display: StatusDisplay,
    title: Vec<u8, Alloc<'a>>,
    pwd: Vec<u8, Alloc<'a>>,
    title_stack: Vec<Vec<u8, Alloc<'a>>, Alloc<'a>>,
    color_scheme: Option<ColorScheme>,
    focused: bool,
    replies: Vec<u8, Alloc<'a>>,
    effects: Effects<'a>,
    /// The dynamic colours and palette OSC 4/10–19/21 set (ghostty's `Terminal.colors`).
    colors: Colors,
    mouse_shape: MouseShape,
    osc: StringBuf<'a>,
    dcs: DcsHandler<'a>,
    apc: Apc<'a>,
    stats: Stats,
    plant: P,
}

/// Tab stops every this many columns.
const TAB_INTERVAL: usize = 8;

impl<'a> Terminal<'a, NoPlant> {
    /// A terminal of `opts.cols` × `opts.rows` blank cells, every buffer in `alloc`.
    pub fn new_in(opts: Opts<'a>, alloc: Alloc<'a>) -> Result<Terminal<'a, NoPlant>, AllocFailure> {
        Terminal::new_planted(opts, alloc, NoPlant)
    }
}

impl<'a, P: Planted<VtPlant>> Terminal<'a, P> {
    /// [`Terminal::new_in`] with its plant seam (D165): `NoPlant` where it ships.
    pub fn new_planted(
        opts: Opts<'a>,
        alloc: Alloc<'a>,
        plant: P,
    ) -> Result<Terminal<'a, P>, AllocFailure> {
        let cols = usize::from(opts.cols.max(1));
        let rows = usize::from(opts.rows.max(1));
        let primary = Screen::new(
            cols,
            rows,
            opts.page_rows.max(1),
            opts.scrollback_bytes,
            false,
            alloc,
            plant,
        )?;
        let mut t = Terminal {
            alloc,
            opts,
            parser: Parser::new(),
            primary,
            alternate: None,
            active: Which::Primary,
            cols,
            rows,
            region: Region { top: 0, bottom: rows - 1, left: 0, right: cols - 1 },
            tabstops: Vec::new_in(alloc),
            modes: ModeState::new(opts.grapheme_cluster),
            flags: Flags::DEFAULT,
            previous_char: None,
            status_display: StatusDisplay::Main,
            title: Vec::new_in(alloc),
            pwd: Vec::new_in(alloc),
            title_stack: Vec::new_in(alloc),
            color_scheme: None,
            focused: true,
            replies: Vec::new_in(alloc),
            effects: Effects::new_in(alloc),
            colors: Colors::default(),
            mouse_shape: MouseShape::Text,
            osc: StringBuf::new(opts.caps.osc, alloc),
            dcs: DcsHandler::new_in(
                DcsOpts { tmux_control_mode: opts.tmux_control_mode, max_bytes: opts.caps.dcs },
                alloc,
            ),
            apc: Apc::new(opts.apc, KittyGraphics::new(opts.graphics, alloc), alloc),
            stats: Stats::default(),
            plant,
        };
        t.tabstops.try_reserve(cols)?;
        t.tabstops.resize(cols, false);
        t.tab_reset();
        Ok(t)
    }

    /// Feeds bytes from the program.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = core::mem::replace(&mut self.parser, Parser::new());
        parser.feed(bytes, self);
        self.parser = parser;
    }

    /// The parser is between sequences (a point where the host may cut the stream).
    pub fn at_rest(&self) -> bool {
        self.parser.at_rest()
    }

    /// The reply bytes written since the last take, for the host to write to the PTY.
    pub fn take_replies(&mut self) -> Vec<u8, Alloc<'a>> {
        core::mem::replace(&mut self.replies, Vec::new_in(self.alloc))
    }

    /// The effects asked for since the last take, in order, with the bytes they carry.
    pub fn take_effects(&mut self) -> Effects<'a> {
        core::mem::replace(&mut self.effects, Effects::new_in(self.alloc))
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The screen shown.
    pub fn screen(&self) -> &Screen<'a, P> {
        match (self.active, &self.alternate) {
            (Which::Alternate, Some(alt)) => alt,
            _ => &self.primary,
        }
    }

    /// The screen shown, writable.
    pub fn screen_mut(&mut self) -> &mut Screen<'a, P> {
        match (self.active, &mut self.alternate) {
            (Which::Alternate, Some(alt)) => alt,
            _ => &mut self.primary,
        }
    }

    pub fn primary(&self) -> &Screen<'a, P> {
        &self.primary
    }

    pub fn alternate(&self) -> Option<&Screen<'a, P>> {
        self.alternate.as_ref()
    }

    pub fn active(&self) -> Which {
        self.active
    }

    pub fn modes(&self) -> &ModeState {
        &self.modes
    }

    pub fn mode(&self, m: Mode) -> bool {
        self.modes.get(m)
    }

    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    pub fn region(&self) -> Region {
        self.region
    }

    pub fn status_display(&self) -> StatusDisplay {
        self.status_display
    }

    pub fn previous_char(&self) -> Option<char> {
        self.previous_char.and_then(char::from_u32)
    }

    pub fn is_tabstop(&self, x: usize) -> bool {
        self.tabstops.get(x).copied().unwrap_or(false)
    }

    /// What was refused, the screens' refusals summed in.
    pub fn stats(&self) -> (Stats, ScreenStats) {
        let mut s = self.primary.stats;
        if let Some(alt) = &self.alternate {
            s.styles_refused += alt.stats.styles_refused;
            s.ext_refused += alt.stats.ext_refused;
            s.alloc_failed += alt.stats.alloc_failed;
            if s.first_failure.is_none() {
                s.first_failure = alt.stats.first_failure;
            }
        }
        (self.stats, s)
    }

    pub fn title(&self) -> &[u8] {
        &self.title
    }

    pub fn pwd(&self) -> &[u8] {
        &self.pwd
    }

    /// Titles pushed by CSI 22 t, oldest first.
    pub fn title_stack_len(&self) -> usize {
        self.title_stack.len()
    }

    /// Sets the title (what OSC 0/2 do), cut at the cap; empty clears.
    pub fn set_title(&mut self, title: &[u8]) {
        let n = title.len().min(self.opts.caps.title);
        self.title.clear();
        if self.title.try_reserve(n).is_ok() {
            self.title.extend_from_slice(&title[..n]);
        }
        self.effect(Effect::TitleChanged);
    }

    /// Sets the working directory (OSC 7 sets it from a local URL's path).
    pub fn set_pwd(&mut self, pwd: &[u8]) {
        self.pwd.clear();
        if self.pwd.try_reserve(pwd.len()).is_ok() {
            self.pwd.extend_from_slice(pwd);
        }
    }

    /// The host's color scheme, for CSI ? 996 n; `None`: no reply.
    pub fn set_color_scheme(&mut self, scheme: Option<ColorScheme>) {
        let changed = scheme != self.color_scheme;
        self.color_scheme = scheme;
        if changed && self.modes.get(Mode::ReportColorScheme) {
            self.report_color_scheme();
        }
    }

    /// Whether the window has focus (mode 1004 reports it when set).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// The cell pixel size, for the size reports and kitty graphics.
    pub fn set_cell_px(&mut self, px: Option<Pixels>) {
        self.opts.cell_px = px;
    }

    /// The palette and dynamic colours (the renderer resolves SGR palette colours with them).
    pub fn colors(&self) -> &Colors {
        &self.colors
    }

    /// The colours, for the host's defaults (the config's palette, foreground, background).
    pub fn colors_mut(&mut self) -> &mut Colors {
        &mut self.colors
    }

    /// The pointer shape OSC 22 last set (ghostty's `Terminal.mouse_shape`, `text` at first).
    pub fn mouse_shape(&self) -> MouseShape {
        self.mouse_shape
    }

    /// The APC state: kitty graphics ([`Apc::graphics`]) and the glyph glossary.
    pub fn apc(&self) -> &Apc<'a> {
        &self.apc
    }

    /// The APC state, for the renderer to clear `dirty` marks.
    pub fn apc_mut(&mut self) -> &mut Apc<'a> {
        &mut self.apc
    }

    /// Which kitty storage belongs to the screen shown.
    pub fn screen_key(&self) -> ScreenKey {
        match self.active {
            Which::Primary => ScreenKey::Primary,
            Which::Alternate => ScreenKey::Alternate,
        }
    }

    /// The screen shown's cells as the kitty placeholder scan reads them
    /// ([`crate::kitty::unicode::placements`]).
    pub fn kitty_cells(&self) -> KittyCells<'_, 'a, P> {
        KittyCells { screen: self.screen() }
    }

    /// The host IO a kitty load waits on ([`Effect::KittyIo`]).
    pub fn kitty_pending(&self) -> Option<Request<'_>> {
        self.apc.graphics().pending()
    }

    /// The host's answer to [`Terminal::kitty_pending`]: the load goes on, and its reply and
    /// cursor movement happen as if the command had just run.
    pub fn kitty_answer(&mut self, answer: Answer<'_>) -> Result<(), UnaskedAnswer> {
        let which = self.apc.graphics().pending_screen().ok_or(UnaskedAnswer)?;
        let cell_px = self.opts.cell_px;
        let screen = match which {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => self.alternate.as_mut().ok_or(UnaskedAnswer)?,
        };
        let out = self.apc.graphics_mut().answer(answer, &mut KittyScreen { screen, cell_px })?;
        let on_active = which == self.screen_key();
        self.kitty_outcome(
            out.reply.map(crate::kitty::apc::Reply::Kitty),
            if on_active { out.cursor } else { None },
            out.waits_on_host,
        );
        Ok(())
    }

    /// The host's answer to [`Effect::ClipboardRead`]: the contents, which are sent base64-encoded
    /// (`OSC 52 ; kind ; base64 ST`).
    pub fn clipboard_reply(&mut self, clipboard: Clipboard, contents: &[u8]) {
        let engine = blaze_base64::STANDARD;
        let Some(n) = engine.encoded_len(contents.len()) else {
            self.stats.replies_dropped += 1;
            return;
        };
        let mut enc: Vec<u8, Alloc<'a>> = Vec::new_in(self.alloc);
        if enc.try_reserve(n).is_err() {
            self.stats.replies_dropped += 1;
            return;
        }
        enc.resize(n, 0);
        match engine.encode_into(contents, &mut enc) {
            Ok(w) => enc.truncate(w),
            Err(_) => {
                self.stats.replies_dropped += 1;
                return;
            }
        }
        self.reply(|o| crate::reply::osc52(o, clipboard, &enc));
    }

    // ---- input options ---------------------------------------------------------------------

    /// The key encoder's options from the modes (key_encode.zig:59 `fromTerminal`); the
    /// platform and option-as-alt are the host's: `input::key::encode(&ev, &t.input_opts(..), out)`.
    pub fn input_opts(
        &self,
        platform: crate::input::key::Platform,
        macos_option_as_alt: crate::input::key::OptionAsAlt,
    ) -> crate::input::key::EncodeOpts {
        let m = &self.modes;
        crate::input::key::EncodeOpts {
            platform,
            cursor_key_application: m.get(Mode::CursorKeys),
            keypad_key_application: m.get(Mode::KeypadKeys),
            backarrow_key_mode: m.get(Mode::BackarrowKey),
            ignore_keypad_with_numlock: m.get(Mode::IgnoreKeypadWithNumlock),
            alt_esc_prefix: m.get(Mode::AltEscPrefix),
            modify_other_keys_state_2: self.flags.modify_other_keys_2,
            kitty_flags: self.screen().kitty.current(),
            macos_option_as_alt,
        }
    }

    /// The mouse encoder's options: the tracking mode and format from the modes; the surface
    /// size and whether a button is down are the host's.
    pub fn mouse_opts(
        &self,
        size: crate::input::mouse::Size,
        any_button_pressed: bool,
    ) -> crate::input::mouse::MouseOpts {
        crate::input::mouse::MouseOpts {
            event: self.flags.mouse_event,
            format: self.flags.mouse_format,
            size,
            any_button_pressed,
        }
    }

    /// The paste encoder's options: bracketed paste (2004).
    pub fn paste_opts(&self) -> crate::input::paste::PasteOpts {
        crate::input::paste::PasteOpts { bracketed: self.modes.get(Mode::BracketedPaste) }
    }

    /// The focus encoder's options: focus reporting (1004).
    pub fn focus_opts(&self) -> crate::input::focus::FocusOpts {
        crate::input::focus::FocusOpts { reporting: self.modes.get(Mode::FocusEvent) }
    }

    /// Takes damage: whether everything needs drawing, and clears every row's dirty mark and
    /// the flag. `dirty` is called with each dirty active row first.
    pub fn take_damage(&mut self, mut dirty: impl FnMut(usize)) -> bool {
        let rows = self.rows;
        let s = self.screen_mut();
        let all = s.all_dirty;
        for y in 0..rows {
            let r = s.r(y);
            let row = s.pages.row_mut(r);
            if row.dirty {
                dirty(y);
                row.dirty = false;
            }
        }
        s.all_dirty = false;
        all
    }

    // ---- what goes out ---------------------------------------------------------------------

    fn effect(&mut self, e: Effect) {
        if !self.effects.push(e, self.opts.caps.effects) {
            self.stats.effects_dropped += 1;
        }
    }

    /// Queues an effect made from text copied into the effect buffer, whole or not at all.
    fn effect_with<const N: usize>(
        &mut self,
        parts: [&[u8]; N],
        make: impl FnOnce([Span; N]) -> Effect,
    ) {
        let caps = self.opts.caps;
        let mark = self.effects.bytes.len();
        let mut spans = [Span::default(); N];
        for (s, p) in spans.iter_mut().zip(parts) {
            match self.effects.stash(p, caps.effect_bytes) {
                Some(span) => *s = span,
                None => {
                    self.effects.bytes.truncate(mark);
                    self.stats.effects_dropped += 1;
                    return;
                }
            }
        }
        if !self.effects.push(make(spans), caps.effects) {
            self.effects.bytes.truncate(mark);
            self.stats.effects_dropped += 1;
        }
    }

    /// Writes a reply with one of [`crate::reply`]'s encoders, whole or not at all: refused by
    /// the allocator or past the cap, it is dropped and counted.
    fn reply(
        &mut self,
        encode: impl FnOnce(crate::reply::Out<'_, 'a>) -> Result<(), AllocFailure>,
    ) {
        reply_into(&mut self.replies, self.opts.caps.replies, &mut self.stats, encode);
    }

    /// Raw reply bytes (a focus report, a colour report written by [`crate::osc::run_colors`]).
    fn reply_bytes(&mut self, parts: &[&[u8]]) {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        if self.replies.len() + n > self.opts.caps.replies || self.replies.try_reserve(n).is_err() {
            self.stats.replies_dropped += 1;
            return;
        }
        for p in parts {
            self.replies.extend_from_slice(p);
        }
    }

    fn unsupported(&mut self) {
        self.stats.unsupported += 1;
    }

    // ---- the screen shown ------------------------------------------------------------------

    fn scr(&mut self) -> &mut Screen<'a, P> {
        self.screen_mut()
    }

    /// The APC state and the screen shown, as kitty graphics takes it.
    fn kitty_split(&mut self) -> (&mut Apc<'a>, KittyScreen<'_, 'a, P>, ScreenKey) {
        let key = self.screen_key();
        let screen = match (self.active, &mut self.alternate) {
            (Which::Alternate, Some(alt)) => alt,
            _ => &mut self.primary,
        };
        (&mut self.apc, KittyScreen { screen, cell_px: self.opts.cell_px }, key)
    }

    /// A screen's kitty storage reset (Screen.zig:396 `reset`).
    fn kitty_reset_screen(&mut self, which: ScreenKey) {
        let cell_px = self.opts.cell_px;
        let screen = match which {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => match self.alternate.as_mut() {
                Some(alt) => alt,
                None => return,
            },
        };
        self.apc.graphics_mut().reset_screen(which, &mut KittyScreen { screen, cell_px });
    }

    /// ED 2 and 22: every placement of the screen shown goes (Terminal.zig:3166).
    fn kitty_erase_display(&mut self) {
        let (apc, mut screen, which) = self.kitty_split();
        apc.graphics_mut().erase_display(which, &mut screen);
    }

    /// What an APC command (or a kitty answer) asked: the reply, then the cursor movement
    /// (stream_terminal.zig `apcEnd`, kitty graphics `display`), then whether it waits.
    fn kitty_outcome(
        &mut self,
        reply: Option<crate::kitty::apc::Reply>,
        cursor: Option<crate::kitty::exec::CursorAfter>,
        waits_on_host: bool,
    ) {
        if let Some(r) = reply {
            let wire = r.encode();
            // ghostty drops a reply of three bytes or fewer (`final.len > 3`).
            if wire.as_bytes().len() > 3 {
                self.reply_bytes(&[wire.as_bytes()]);
            }
        }
        if let Some(c) = cursor {
            for _ in 0..c.index {
                self.index();
            }
            let y = self.scr().cursor.y;
            // ghostty passes the 0-based row as the 1-based one (graphics_exec.zig `display`).
            self.set_cursor_pos(y, c.col_1based as usize);
        }
        if waits_on_host {
            self.effect(Effect::KittyIo);
        }
    }
}

/// [`Terminal::reply`] on the reply buffer alone, for a caller that borrows another field.
fn reply_into<'a>(
    replies: &mut Vec<u8, Alloc<'a>>,
    cap: usize,
    stats: &mut Stats,
    encode: impl FnOnce(crate::reply::Out<'_, 'a>) -> Result<(), AllocFailure>,
) {
    let start = replies.len();
    if encode(replies).is_err() || replies.len() > cap {
        replies.truncate(start);
        stats.replies_dropped += 1;
    }
}
