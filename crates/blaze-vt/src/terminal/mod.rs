//! The terminal: two screens, the modes, the scrolling region, tab stops and the state of a
//! session, driven by the parser as its [`Sink`](crate::parser::Sink). Ports ghostty's
//! Terminal.zig and the dispatch of stream.zig / stream_terminal.zig (vt-inventory.md §2–5,
//! §11, §12).
//!
//! Sans-IO: [`Terminal::feed`] takes bytes and returns nothing. What the program asked to have
//! written back (device attributes, cursor reports, mode reports) is bytes in a reply buffer
//! ([`Terminal::take_replies`]); what the host is asked to do (ring the bell, retitle the
//! window) is an [`Effect`] list ([`Terminal::take_effects`]). Both are capped by
//! [`Caps`]: past a cap a reply or effect is refused and counted in [`Stats`].

mod dispatch;
pub mod modes;
mod ops;
mod print;

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::parser::Parser;
use crate::plant::VtPlant;
use crate::screen::{Screen, ScreenStats};

pub use modes::{Mode, ModeState, MouseEvents, MouseFormat};

/// What the host is asked to do. The other lanes add variants (clipboard, notifications, …).
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
    /// An OSC's bytes.
    pub osc: usize,
    /// A DCS's bytes.
    pub dcs: usize,
    /// An APC's bytes (kitty graphics send images in them).
    pub apc: usize,
    /// The title's bytes (ghostty truncates at 1024, stream_terminal.zig `windowTitle`).
    pub title: usize,
    /// Titles CSI 22 t can push.
    pub title_stack: usize,
}

impl Caps {
    pub const DEFAULT: Caps = Caps {
        replies: 64 << 10,
        effects: 1024,
        osc: 64 << 10,
        dcs: 64 << 10,
        apc: 16 << 20,
        title: 1024,
        title_stack: 10,
    };
}

/// How to make a terminal: the size and scrollback are the caller's choice; the rest defaults.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// A DCS's hook, kept until it is unhooked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DcsHead {
    pub intermediates: [u8; crate::parser::MAX_INTERMEDIATES],
    pub n_intermediates: u8,
    pub params: [u16; crate::parser::MAX_PARAMS],
    pub n_params: u8,
    pub final_byte: u8,
}

/// A string sequence being gathered (OSC, DCS, APC), capped.
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
    effects: Vec<Effect, Alloc<'a>>,
    osc: StringBuf<'a>,
    dcs: StringBuf<'a>,
    dcs_head: Option<DcsHead>,
    apc: StringBuf<'a>,
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
            effects: Vec::new_in(alloc),
            osc: StringBuf::new(opts.caps.osc, alloc),
            dcs: StringBuf::new(opts.caps.dcs, alloc),
            dcs_head: None,
            apc: StringBuf::new(opts.caps.apc, alloc),
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

    /// The effects asked for since the last take, in order.
    pub fn take_effects(&mut self) -> Vec<Effect, Alloc<'a>> {
        core::mem::replace(&mut self.effects, Vec::new_in(self.alloc))
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

    /// The screen shown, writable (the other lanes' hyperlink and prompt calls).
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

    /// Sets the title (OSC 0/2, the OSC lane's call), cut at the cap; empty clears.
    pub fn set_title(&mut self, title: &[u8]) {
        let n = title.len().min(self.opts.caps.title);
        self.title.clear();
        if self.title.try_reserve(n).is_ok() {
            self.title.extend_from_slice(&title[..n]);
        }
        self.effect(Effect::TitleChanged);
    }

    /// Sets the working directory (OSC 7, the OSC lane's call).
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

    /// The cell pixel size, for the size reports.
    pub fn set_cell_px(&mut self, px: Option<Pixels>) {
        self.opts.cell_px = px;
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
        if self.effects.len() >= self.opts.caps.effects || self.effects.try_reserve(1).is_err() {
            self.stats.effects_dropped += 1;
            return;
        }
        self.effects.push(e);
    }

    /// Writes a reply with one of [`crate::reply`]'s encoders, whole or not at all: refused by
    /// the allocator or past the cap, it is dropped and counted.
    fn reply(
        &mut self,
        encode: impl FnOnce(crate::reply::Out<'_, 'a>) -> Result<(), AllocFailure>,
    ) {
        let start = self.replies.len();
        if encode(&mut self.replies).is_err() || self.replies.len() > self.opts.caps.replies {
            self.replies.truncate(start);
            self.stats.replies_dropped += 1;
        }
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
}
