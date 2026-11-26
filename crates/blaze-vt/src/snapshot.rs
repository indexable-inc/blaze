//! Canonical, portable VT state snapshot & restore for [`Terminal`](crate::terminal::Terminal).
//!
//! Captures a terminal's full observable state as minimal, canonical VT byte streams
//! (`primary_vt` and optional `alt_vt`) plus compact non-VT metadata (bit-packed per-row
//! soft-wrap flags for faithful resize reflow, deferred-wrap cursor state, and non-replayable
//! mode bits). Nothing internal to [`PageList`](crate::page::PageList),
//! [`StyleSet`](crate::screen::StyleSet), or [`ExtTable`](crate::screen::ExtTable) is persisted,
//! so snapshots remain portable across internal layout or schema updates.

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::planted::Planted;
use blaze_own::{AllocFailure, Vec};

use crate::color::{self, Colors};
use crate::osc::MouseShape;
use crate::page::{RowPrompt, SemanticContent, Wide};
use crate::plant::VtPlant;
use crate::screen::{Charset, CursorShape, LinkId, ProtectedMode, Screen, Slot};
use crate::style::{Color, Rgb, Style, Underline};
use crate::terminal::modes::MODES;
use crate::terminal::{Mode, MouseEvents, MouseFormat, Terminal, Which};

/// Maximum parameters per CSI SGR sequence emitted (well below [`crate::parser::MAX_PARAMS`]).
const MAX_SGR_BATCH_PARAMS: usize = 16;

// Non-replay mode bits stored in `VtSnapshot::non_replay_modes`.
const NR_ALT_LEGACY: u8 = 1 << 0; // ?47
const NR_ALT: u8 = 1 << 1; // ?1047
const NR_ALT_SAVE_CLEAR: u8 = 1 << 2; // ?1049
const NR_SAVE_CURSOR: u8 = 1 << 3; // ?1048
const NR_COL_132: u8 = 1 << 4; // ?3

// Per-row bit flags stored in `primary_soft_wraps` / `alt_soft_wraps` (4 bits per row, 2 rows/byte).
const ROW_WRAP: u8 = 1 << 0;
const ROW_WRAP_CONT: u8 = 1 << 1;
const ROW_SPACER_HEAD: u8 = 1 << 2;

/// Why [`restore`] could not apply a snapshot to a terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestoreError {
    /// The target terminal's dimensions do not match the snapshot's `(cols, rows)`.
    DimensionMismatch {
        snapshot_cols: u16,
        snapshot_rows: u16,
        terminal_cols: usize,
        terminal_rows: usize,
    },
    /// The terminal's allocator refused an allocation while restoring.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for RestoreError {
    fn from(f: AllocFailure) -> RestoreError {
        RestoreError::Alloc(f)
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RestoreError::DimensionMismatch {
                snapshot_cols,
                snapshot_rows,
                terminal_cols,
                terminal_rows,
            } => write!(
                f,
                "blaze-vt snapshot restore dimension mismatch: snapshot is {snapshot_cols}x{snapshot_rows}, terminal is {terminal_cols}x{terminal_rows}"
            ),
            RestoreError::Alloc(e) => write!(f, "blaze-vt snapshot restore allocation failed: {e:?}"),
        }
    }
}

/// A canonical, portable VT snapshot of a [`Terminal`].
///
/// The VT streams (`primary_vt`, `alt_vt`) are standard, minimal escape-sequence streams that
/// reconstruct scrollback, visible rows, UTF-8 text, wide characters, combining graphemes, SGR
/// styles (256-color palette, 24-bit RGB, underline styles and color, all flags), OSC 8
/// hyperlinks, OSC 4/10/11/12 palette & dynamic colors, OSC 2 title, OSC 7 cwd, OSC 22 pointer
/// shape, OSC 133 prompt marks, cursor position/shape/visibility, margins, tabstops, charsets,
/// kitty keyboard flags, and DEC/ANSI modes.
///
/// `primary_soft_wraps` and `alt_soft_wraps` bit-pack per-row reflow flags (4 bits per row:
/// `wrap`, `wrap_continuation`, and trailing `SpacerHead`) so resize reflow reproduces every
/// wrap state, including states left behind by scrolling or erasing.
#[derive(Debug)]
pub struct VtSnapshot<A: Allocator> {
    pub cols: u16,
    pub rows: u16,
    pub active_alt: bool,
    /// Minimal canonical VT byte stream reconstructing the primary screen's scrollback, visible
    /// rows, and state.
    pub primary_vt: Vec<u8, A>,
    /// Optional canonical VT byte stream for the alternate screen when active or non-empty.
    pub alt_vt: Option<Vec<u8, A>>,
    /// Bit-packed per-row soft-wrap flags for the primary screen (4 bits per row:
    /// bit 0 = `wrap`, bit 1 = `wrap_continuation`, bit 2 = trailing `SpacerHead`).
    pub primary_soft_wraps: Vec<u8, A>,
    /// Bit-packed per-row soft-wrap flags for the alternate screen (same 4-bit layout).
    pub alt_soft_wraps: Vec<u8, A>,
    /// Deferred-wrap (`pending_wrap`) cursor flag on the primary screen.
    pub primary_pending_wrap: bool,
    /// Deferred-wrap (`pending_wrap`) cursor flag on the alternate screen.
    pub alt_pending_wrap: bool,
    /// Bitmask of modes (`?47`, `?1047`, `?1049`, `?1048`, `?3`) whose direct VT replay would
    /// clobber saved cursor or resize/wipe the screen.
    pub non_replay_modes: u8,
}

impl<A: Allocator, B: Allocator> PartialEq<VtSnapshot<B>> for VtSnapshot<A> {
    fn eq(&self, other: &VtSnapshot<B>) -> bool {
        self.cols == other.cols
            && self.rows == other.rows
            && self.active_alt == other.active_alt
            && self.primary_vt.as_slice() == other.primary_vt.as_slice()
            && self.alt_vt.as_deref() == other.alt_vt.as_deref()
            && self.primary_soft_wraps.as_slice() == other.primary_soft_wraps.as_slice()
            && self.alt_soft_wraps.as_slice() == other.alt_soft_wraps.as_slice()
            && self.primary_pending_wrap == other.primary_pending_wrap
            && self.alt_pending_wrap == other.alt_pending_wrap
            && self.non_replay_modes == other.non_replay_modes
    }
}

impl<A: Allocator> Eq for VtSnapshot<A> {}

impl<A: Allocator> VtSnapshot<A> {
    /// Whether primary row `r` (0 = oldest scrollback row) soft-wraps into `r + 1`.
    pub fn primary_row_wrap(&self, r: usize) -> bool {
        row_flags_get(&self.primary_soft_wraps, r) & ROW_WRAP != 0
    }

    /// Whether primary row `r` is the continuation of a soft-wrapped row above it.
    pub fn primary_row_wrap_continuation(&self, r: usize) -> bool {
        row_flags_get(&self.primary_soft_wraps, r) & ROW_WRAP_CONT != 0
    }

    /// Whether alternate row `r` soft-wraps into `r + 1`.
    pub fn alt_row_wrap(&self, r: usize) -> bool {
        row_flags_get(&self.alt_soft_wraps, r) & ROW_WRAP != 0
    }

    /// Whether alternate row `r` is the continuation of a soft-wrapped row above it.
    pub fn alt_row_wrap_continuation(&self, r: usize) -> bool {
        row_flags_get(&self.alt_soft_wraps, r) & ROW_WRAP_CONT != 0
    }
}

/// Captures a canonical, portable VT snapshot of `term` in `alloc`.
pub fn capture<A: Allocator + Clone, P: Planted<VtPlant>>(
    term: &Terminal<'_, P>,
    alloc: A,
) -> Result<VtSnapshot<A>, AllocFailure> {
    let plant = term.plant();
    let cols = term.cols() as u16;
    let rows = term.rows() as u16;
    let active_alt = term.active() == Which::Alternate;

    let mut primary_w = Writer::new(alloc.clone());
    emit_global_oscs(&mut primary_w, term);
    emit_screen(&mut primary_w, term, term.primary(), !active_alt, plant);
    let primary_vt = primary_w.finish()?;

    let primary_soft_wraps = if plant.has(VtPlant::SnapshotDropsSoftWrap) {
        pack_zero_wraps(term.primary().pages.total(), alloc.clone())?
    } else {
        pack_soft_wraps(term.primary(), alloc.clone())?
    };

    let (alt_vt, alt_soft_wraps, alt_pending_wrap) = match term.alternate() {
        Some(alt) if active_alt || screen_is_non_empty(alt) => {
            let mut alt_w = Writer::new(alloc.clone());
            emit_screen(&mut alt_w, term, alt, active_alt, plant);
            let vt = alt_w.finish()?;
            let wraps = pack_soft_wraps(alt, alloc)?;
            (Some(vt), wraps, alt.cursor.pending_wrap)
        }
        _ => (None, Vec::new_in(alloc), false),
    };

    let non_replay_modes = pack_non_replay_modes(term);

    Ok(VtSnapshot {
        cols,
        rows,
        active_alt,
        primary_vt,
        alt_vt,
        primary_soft_wraps,
        alt_soft_wraps,
        primary_pending_wrap: term.primary().cursor.pending_wrap,
        alt_pending_wrap,
        non_replay_modes,
    })
}

/// Restores `snap` into `term`.
///
/// The terminal is reset, the canonical VT byte streams are replayed through [`Terminal::feed`],
/// soft-wrap / deferred-wrap / non-replay mode metadata is applied, and any spurious replies or
/// effects generated during replay are cleared so the terminal resumes cleanly.
pub fn restore<A: Allocator, P: Planted<VtPlant>>(
    term: &mut Terminal<'_, P>,
    snap: &VtSnapshot<A>,
) -> Result<(), RestoreError> {
    if term.cols() != usize::from(snap.cols) || term.rows() != usize::from(snap.rows) {
        return Err(RestoreError::DimensionMismatch {
            snapshot_cols: snap.cols,
            snapshot_rows: snap.rows,
            terminal_cols: term.cols(),
            terminal_rows: term.rows(),
        });
    }

    term.reset_for_restore();

    if !snap.active_alt {
        if let Some(alt_vt) = &snap.alt_vt {
            term.switch_screen_for_restore(Which::Alternate)?;
            term.feed(alt_vt);
            if let Some(alt) = term.alternate_mut() {
                apply_soft_wraps(alt, &snap.alt_soft_wraps);
                alt.cursor.pending_wrap = snap.alt_pending_wrap;
            }
            term.switch_screen_for_restore(Which::Primary)?;
            // Alt-screen replay must not leak mode changes into primary replay.
            term.modes_mut().reset();
        }
        term.feed(&snap.primary_vt);
        apply_soft_wraps(term.primary_mut(), &snap.primary_soft_wraps);
        term.primary_mut().cursor.pending_wrap = snap.primary_pending_wrap;
    } else {
        term.feed(&snap.primary_vt);
        apply_soft_wraps(term.primary_mut(), &snap.primary_soft_wraps);
        term.primary_mut().cursor.pending_wrap = snap.primary_pending_wrap;

        term.switch_screen_for_restore(Which::Alternate)?;
        if let Some(alt_vt) = &snap.alt_vt {
            term.feed(alt_vt);
        }
        if let Some(alt) = term.alternate_mut() {
            apply_soft_wraps(alt, &snap.alt_soft_wraps);
            alt.cursor.pending_wrap = snap.alt_pending_wrap;
        }
    }

    apply_non_replay_modes(term, snap.non_replay_modes);

    // Clear any replies (e.g. focus/size/color-scheme reports) and effects queued by replay.
    let _ = term.take_replies();
    let _ = term.take_effects();

    let s = term.screen_mut();
    s.all_dirty = true;
    s.pages.mark_all_dirty();
    Ok(())
}

// ---- bit-packed soft-wrap metadata -------------------------------------------------------------

fn row_flags_get(bytes: &[u8], r: usize) -> u8 {
    let shift = (r & 1) * 4;
    bytes.get(r / 2).map_or(0, |&b| (b >> shift) & 0x0F)
}

fn row_flags_set(bytes: &mut [u8], r: usize, flags: u8) {
    let shift = (r & 1) * 4;
    bytes[r / 2] |= (flags & 0x0F) << shift;
}

fn pack_zero_wraps<A: Allocator>(total_rows: usize, alloc: A) -> Result<Vec<u8, A>, AllocFailure> {
    let byte_len = total_rows.div_ceil(2);
    let mut out = Vec::try_with_capacity_in(byte_len, alloc)?;
    out.resize(byte_len, 0);
    Ok(out)
}

fn pack_soft_wraps<A: Allocator, P: Planted<VtPlant>>(
    screen: &Screen<'_, P>,
    alloc: A,
) -> Result<Vec<u8, A>, AllocFailure> {
    let total = screen.pages.total();
    let cols = screen.cols();
    let mut out = pack_zero_wraps(total, alloc)?;
    for r in 0..total {
        let row = screen.pages.row(r);
        let mut flags = 0u8;
        if row.wrap {
            flags |= ROW_WRAP;
        }
        if row.wrap_continuation {
            flags |= ROW_WRAP_CONT;
        }
        if cols > 0 && screen.pages.cell(r, cols - 1).wide() == Wide::SpacerHead {
            flags |= ROW_SPACER_HEAD;
        }
        if flags != 0 {
            row_flags_set(&mut out, r, flags);
        }
    }
    Ok(out)
}

fn apply_soft_wraps<P: Planted<VtPlant>>(screen: &mut Screen<'_, P>, packed: &[u8]) {
    let total = screen.pages.total();
    let cols = screen.cols();
    for r in 0..total {
        let flags = row_flags_get(packed, r);
        let row = screen.pages.row_mut(r);
        row.wrap = flags & ROW_WRAP != 0;
        row.wrap_continuation = flags & ROW_WRAP_CONT != 0;
        if cols > 0 && flags & ROW_SPACER_HEAD != 0 {
            let mut c = screen.pages.cell(r, cols - 1);
            if c.codepoint() == 0 && !c.is_bg() && c.wide() == Wide::Narrow {
                c.set_wide(Wide::SpacerHead);
                screen.pages.set_cell(r, cols - 1, c);
            }
        }
    }
}

fn pack_non_replay_modes<P: Planted<VtPlant>>(term: &Terminal<'_, P>) -> u8 {
    let mut bits = 0u8;
    if term.mode(Mode::AltScreenLegacy) {
        bits |= NR_ALT_LEGACY;
    }
    if term.mode(Mode::AltScreen) {
        bits |= NR_ALT;
    }
    if term.mode(Mode::AltScreenSaveCursorClear) {
        bits |= NR_ALT_SAVE_CLEAR;
    }
    if term.mode(Mode::SaveCursor) {
        bits |= NR_SAVE_CURSOR;
    }
    if term.mode(Mode::Col132) {
        bits |= NR_COL_132;
    }
    bits
}

fn apply_non_replay_modes<P: Planted<VtPlant>>(term: &mut Terminal<'_, P>, bits: u8) {
    let m = term.modes_mut();
    m.set(Mode::AltScreenLegacy, bits & NR_ALT_LEGACY != 0);
    m.set(Mode::AltScreen, bits & NR_ALT != 0);
    m.set(Mode::AltScreenSaveCursorClear, bits & NR_ALT_SAVE_CLEAR != 0);
    m.set(Mode::SaveCursor, bits & NR_SAVE_CURSOR != 0);
    m.set(Mode::Col132, bits & NR_COL_132 != 0);
}

// ---- writer ------------------------------------------------------------------------------------

struct Writer<A: Allocator> {
    out: Vec<u8, A>,
    failed: Option<AllocFailure>,
}

impl<A: Allocator> Writer<A> {
    fn new(alloc: A) -> Self {
        Self { out: Vec::new_in(alloc), failed: None }
    }

    fn finish(self) -> Result<Vec<u8, A>, AllocFailure> {
        match self.failed {
            None => Ok(self.out),
            Some(why) => Err(why),
        }
    }

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

    fn dec(&mut self, mut v: usize) {
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

    fn utf8(&mut self, cp: u32) {
        if let Some(ch) = char::from_u32(cp) {
            let mut buf = [0u8; 4];
            self.bytes(ch.encode_utf8(&mut buf).as_bytes());
        }
    }

    fn hex_upper(&mut self, b: u8) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        self.bytes(&[HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xF)]]);
    }
}

// ---- global OSC metadata (palette, title, cwd, pointer shape) ----------------------------------

fn emit_global_oscs<A: Allocator, P: Planted<VtPlant>>(w: &mut Writer<A>, term: &Terminal<'_, P>) {
    emit_colors(w, term.colors());

    let title = term.title();
    if !title.is_empty() {
        w.bytes(b"\x1b]2;");
        w.bytes(title);
        w.bytes(b"\x1b\\");
    }

    let pwd = term.pwd();
    if !pwd.is_empty() && pwd[0] == b'/' {
        w.bytes(b"\x1b]7;file://localhost");
        emit_percent_encoded(w, pwd);
        w.bytes(b"\x1b\\");
    }

    let shape = term.mouse_shape();
    if shape != MouseShape::Text {
        w.bytes(b"\x1b]22;");
        w.bytes(mouse_shape_name(shape));
        w.bytes(b"\x1b\\");
    }
}

fn emit_colors<A: Allocator>(w: &mut Writer<A>, colors: &Colors) {
    for i in colors.palette.mask.iter() {
        let c = colors.palette.current[usize::from(i)];
        w.bytes(b"\x1b]4;");
        w.dec(usize::from(i));
        w.byte(b';');
        w.bytes(&color::rgb8(c));
        w.bytes(b"\x1b\\");
    }
    if let Some(c) = colors.foreground.override_ {
        w.bytes(b"\x1b]10;");
        w.bytes(&color::rgb8(c));
        w.bytes(b"\x1b\\");
    }
    if let Some(c) = colors.background.override_ {
        w.bytes(b"\x1b]11;");
        w.bytes(&color::rgb8(c));
        w.bytes(b"\x1b\\");
    }
    if let Some(c) = colors.cursor.override_ {
        w.bytes(b"\x1b]12;");
        w.bytes(&color::rgb8(c));
        w.bytes(b"\x1b\\");
    }
}

fn emit_percent_encoded<A: Allocator>(w: &mut Writer<A>, path: &[u8]) {
    for &b in path {
        // Keep unreserved URI path chars literal; encode `%`, `?`, `#`, whitespace, control and
        // non-ASCII bytes so `OSC 7` round-trips identically through `url_percent_decode`.
        if b > 0x20 && b < 0x7F && !matches!(b, b'%' | b'?' | b'#') {
            w.byte(b);
        } else {
            w.byte(b'%');
            w.hex_upper(b);
        }
    }
}

fn mouse_shape_name(shape: MouseShape) -> &'static [u8] {
    use MouseShape as M;
    match shape {
        M::Default => b"default",
        M::ContextMenu => b"context-menu",
        M::Help => b"help",
        M::Pointer => b"pointer",
        M::Progress => b"progress",
        M::Wait => b"wait",
        M::Cell => b"cell",
        M::Crosshair => b"crosshair",
        M::Text => b"text",
        M::VerticalText => b"vertical-text",
        M::Alias => b"alias",
        M::Copy => b"copy",
        M::Move => b"move",
        M::NoDrop => b"no-drop",
        M::NotAllowed => b"not-allowed",
        M::Grab => b"grab",
        M::Grabbing => b"grabbing",
        M::AllScroll => b"all-scroll",
        M::ColResize => b"col-resize",
        M::RowResize => b"row-resize",
        M::NResize => b"n-resize",
        M::EResize => b"e-resize",
        M::SResize => b"s-resize",
        M::WResize => b"w-resize",
        M::NeResize => b"ne-resize",
        M::NwResize => b"nw-resize",
        M::SeResize => b"se-resize",
        M::SwResize => b"sw-resize",
        M::EwResize => b"ew-resize",
        M::NsResize => b"ns-resize",
        M::NeswResize => b"nesw-resize",
        M::NwseResize => b"nwse-resize",
        M::ZoomIn => b"zoom-in",
        M::ZoomOut => b"zoom-out",
    }
}

// ---- screen content & state emission -----------------------------------------------------------

fn row_has_content<P: Planted<VtPlant>>(screen: &Screen<'_, P>, r: usize) -> bool {
    if screen.pages.row(r).prompt != RowPrompt::None {
        return true;
    }
    screen
        .pages
        .cells(r)
        .iter()
        .any(|c| c.has_text() || c.is_bg() || c.wide() != Wide::Narrow)
}

fn screen_is_non_empty<P: Planted<VtPlant>>(screen: &Screen<'_, P>) -> bool {
    let c = screen.cursor;
    if c.x != 0
        || c.y != 0
        || c.pending_wrap
        || c.protected
        || c.shape != CursorShape::Block
        || !c.style.is_default()
        || c.hyperlink != 0
        || c.semantic != SemanticContent::Output
        || screen.saved.is_some()
        || screen.charset != crate::screen::Charsets::default()
        || screen.protected_mode != ProtectedMode::Off
        || screen.kitty.current().bits() != 0
        || screen.pages.scrollback() > 0
    {
        return true;
    }
    (0..screen.pages.total()).any(|r| {
        let row = screen.pages.row(r);
        row.wrap || row.wrap_continuation || row_has_content(screen, r)
    })
}

fn needs_grapheme_mode(base: u32, extra: &[u32], wide: Wide) -> bool {
    if extra.is_empty() {
        return false;
    }
    if extra.iter().any(|&c| crate::terminal::cp_width(c) != 0) {
        return true;
    }
    let bw = crate::terminal::cp_width(base);
    (wide == Wide::Wide && bw == 1) || (wide == Wide::Narrow && bw == 2)
}

/// Emits one screen's scrollback + visible rows, per-screen state (saved cursor, charsets,
/// kitty flags, cursor shape/style/hyperlink/position), and — when `emit_global` is true —
/// the terminal-global modes, flags, tabstops, and scrolling margins.
fn emit_screen<A: Allocator, P: Planted<VtPlant>>(
    w: &mut Writer<A>,
    term: &Terminal<'_, P>,
    screen: &Screen<'_, P>,
    emit_global: bool,
    plant: P,
) {
    let drop_style = plant.has(VtPlant::SnapshotDropsStyle);
    let drop_cursor = plant.has(VtPlant::SnapshotDropsCursor);

    let cols = screen.cols();
    let rows = screen.rows();
    let total = screen.pages.total();
    let scrollback = screen.pages.scrollback();

    let emit_rows = if scrollback > 0 {
        total
    } else {
        (0..total).rev().find(|&r| row_has_content(screen, r)).map_or(0, |r| r + 1)
    };

    let mut cur_x = 0usize;
    let mut cur_y = 0usize;
    let mut cur_pending_wrap = false;
    let mut cur_style = Style::default();
    let mut cur_link = 0u32;
    let mut cur_protected = false;
    let mut cur_semantic = SemanticContent::Output;
    let mut in_spacer_wrap = false;
    let mut in_natural_wrap = false;
    let mut cursor_moved = false;

    for r in 0..emit_rows {
        let cells = screen.pages.cells(r);
        let row_meta = *screen.pages.row(r);

        if !in_spacer_wrap && !in_natural_wrap && screen.semantic_prompt.seen {
            match row_meta.prompt {
                RowPrompt::Prompt => {
                    w.bytes(b"\x1b]133;P;k=i\x1b\\");
                    cur_semantic = SemanticContent::Prompt;
                }
                RowPrompt::Continuation => {
                    w.bytes(b"\x1b]133;P;k=c\x1b\\");
                    cur_semantic = SemanticContent::Prompt;
                }
                RowPrompt::None => {}
            }
        }

        let mut x = 0usize;
        while x < cols {
            let cell = cells[x];
            match cell.wide() {
                Wide::SpacerTail => {
                    x += 1;
                    continue;
                }
                Wide::SpacerHead => {
                    // Only emit SpacerHead via natural wide-char wrap when the next row starts
                    // with a wide character.
                    if x == cols - 1
                        && r + 1 < emit_rows
                        && row_meta.wrap
                        && screen.pages.cell(r + 1, 0).wide() == Wide::Wide
                        && screen.pages.cell(r + 1, 0).has_text()
                        && screen.pages.row(r + 1).prompt == RowPrompt::None
                    {
                        move_x(w, &mut cur_x, &mut cur_pending_wrap, cols - 1);
                        cursor_moved = true;
                        in_spacer_wrap = true;
                        break;
                    }
                    x += 1;
                    continue;
                }
                Wide::Narrow | Wide::Wide => {}
            }

            if cell.is_bg() {
                let style_id = cell.style_id();
                let mut end = x + 1;
                while end < cols && cells[end].is_bg() && cells[end].style_id() == style_id {
                    end += 1;
                }
                move_x(w, &mut cur_x, &mut cur_pending_wrap, x);
                cursor_moved = true;
                if !drop_style {
                    let target_bg = screen.style(style_id).bg;
                    if cur_style.bg != target_bg {
                        let next_style = Style { bg: target_bg, ..cur_style };
                        emit_style_diff(w, cur_style, next_style);
                        cur_style = next_style;
                    }
                }
                let count = end - x;
                if count == 1 {
                    w.bytes(b"\x1b[X");
                } else {
                    w.bytes(b"\x1b[");
                    w.dec(count);
                    w.byte(b'X');
                }
                cur_pending_wrap = false;
                x = end;
                continue;
            }

            if !cell.has_text() {
                x += 1;
                continue;
            }

            let wrapping_now = in_spacer_wrap || in_natural_wrap;
            if !wrapping_now {
                move_x(w, &mut cur_x, &mut cur_pending_wrap, x);
            }
            cursor_moved = true;

            if !drop_style {
                let target_style = *screen.style(cell.style_id());
                emit_style_diff(w, cur_style, target_style);
                cur_style = target_style;
            }

            let target_link = screen.cell_link(cell);
            emit_hyperlink(w, screen, &mut cur_link, target_link);

            let target_prot = cell.protected();
            if cur_protected != target_prot {
                w.bytes(if target_prot { b"\x1b[1\"q" } else { b"\x1b[2\"q" });
                cur_protected = target_prot;
            }

            if screen.semantic_prompt.seen {
                let target_sem = cell.semantic();
                if cur_semantic != target_sem {
                    emit_semantic(w, target_sem, row_meta.prompt == RowPrompt::Continuation);
                    cur_semantic = target_sem;
                }
            }

            let cp = cell.codepoint();
            let extra = screen.graphemes(cell);
            let need_2027 = needs_grapheme_mode(cp, extra, cell.wide());
            if need_2027 {
                w.bytes(b"\x1b[?2027h");
            }
            if cp < 0x10 || cp == 0x1B {
                w.byte(b' ');
            } else {
                w.utf8(cp);
            }
            for &g in extra {
                w.utf8(g);
            }
            if need_2027 {
                w.bytes(b"\x1b[?2027l");
            }

            let width = if cell.wide() == Wide::Wide { 2 } else { 1 };
            let scrolled_on_spacer = in_spacer_wrap && cur_y + 1 == rows;
            if wrapping_now {
                if cur_y + 1 < rows {
                    cur_y += 1;
                }
                in_spacer_wrap = false;
                in_natural_wrap = false;
            }

            if x + width >= cols {
                cur_x = cols - 1;
                cur_pending_wrap = true;
            } else {
                cur_x = x + width;
                cur_pending_wrap = false;
            }

            // If a SpacerHead wrap scrolled a new row while `bg != None`, `fill_blank` painted
            // cols `width..cols` with BCE background cells; clear them back to `Cell::BLANK`.
            if scrolled_on_spacer && cur_style.bg != Color::None && !cur_pending_wrap {
                w.bytes(b"\x1b[49m\x1b[K");
                cur_style.bg = Color::None;
            }

            x += width;
        }

        if r + 1 < emit_rows && !in_spacer_wrap {
            let next_row = screen.pages.row(r + 1);
            let next_c0 = screen.pages.cell(r + 1, 0);
            let can_natural_wrap = cur_pending_wrap
                && cur_x == cols - 1
                && row_meta.wrap
                && next_row.wrap_continuation
                && next_c0.has_text()
                && next_row.prompt == RowPrompt::None
                && (cur_y + 1 < rows
                    || (!drop_style && screen.style(next_c0.style_id()).bg == Color::None));

            if can_natural_wrap {
                in_natural_wrap = true;
            } else {
                if cur_semantic != SemanticContent::Output {
                    w.bytes(b"\x1b]133;D\x1b\\");
                    cur_semantic = SemanticContent::Output;
                }
                if cur_y + 1 == rows && cur_style.bg != Color::None {
                    w.bytes(b"\x1b[49m");
                    cur_style.bg = Color::None;
                }
                if cur_x != 0 || cur_pending_wrap {
                    w.bytes(b"\r\n");
                } else {
                    w.byte(b'\n');
                }
                cur_x = 0;
                cur_pending_wrap = false;
                if cur_y + 1 < rows {
                    cur_y += 1;
                }
                cursor_moved = true;
            }
        }
    }

    // Close any open hyperlink and protection/semantic state left from row emission.
    emit_hyperlink(w, screen, &mut cur_link, 0);
    if cur_protected {
        w.bytes(b"\x1b[2\"q");
        cur_protected = false;
    }
    if cur_semantic != SemanticContent::Output {
        w.bytes(b"\x1b]133;D\x1b\\");
    }

    // Tabstops (terminal-global; moves cursor horizontally, so emit before saved/final cursor).
    if emit_global && has_custom_tabstops(term) {
        w.bytes(b"\x1b[3g");
        for x in 0..cols {
            if term.is_tabstop(x) {
                emit_cha(w, x);
                w.bytes(b"\x1bH");
                cur_x = x;
                cur_pending_wrap = false;
                cursor_moved = true;
            }
        }
    }

    // Saved cursor (DECSC / ESC 7), emitted while margins are still full-screen so any
    // `(saved.x, saved.y)` is reachable even when `saved.origin` is true.
    let mut cur_charset = crate::screen::Charsets::default();
    if let Some(saved) = screen.saved {
        if saved.origin {
            w.bytes(b"\x1b[?6h");
        }
        emit_cup(w, saved.y, saved.x);
        if !drop_style {
            emit_style_diff(w, cur_style, saved.style);
            cur_style = saved.style;
        }
        if cur_protected != saved.protected {
            w.bytes(if saved.protected { b"\x1b[1\"q" } else { b"\x1b[2\"q" });
            cur_protected = saved.protected;
        }
        emit_charsets(w, &mut cur_charset, saved.charset);
        w.bytes(b"\x1b7");
        if saved.origin {
            w.bytes(b"\x1b[?6l");
            // DECRST ?6 moves the cursor home (ops.rs `set_mode(Mode::Origin)`).
            cur_x = 0;
            cur_y = 0;
        } else {
            cur_x = saved.x;
            cur_y = saved.y;
        }
        cur_pending_wrap = false;
        cursor_moved = true;
    }

    // Per-screen charsets, kitty keyboard flags, protected mode, cursor shape.
    emit_charsets(w, &mut cur_charset, screen.charset);

    let kitty_bits = screen.kitty.current().bits();
    if kitty_bits != 0 {
        w.bytes(b"\x1b[=");
        w.dec(usize::from(kitty_bits));
        w.bytes(b";1u");
    }

    // Protected mode + cursor.protected.
    match (screen.protected_mode, screen.cursor.protected) {
        (ProtectedMode::Off, false) => {
            if cur_protected {
                w.bytes(b"\x1b[2\"q");
            }
        }
        (ProtectedMode::Iso, true) => w.bytes(b"\x1bV"),
        (ProtectedMode::Iso, false) => w.bytes(b"\x1bV\x1bW"),
        (ProtectedMode::Dec, true) => w.bytes(b"\x1b[1\"q"),
        (ProtectedMode::Dec, false) => w.bytes(b"\x1b[1\"q\x1b[2\"q"),
        (ProtectedMode::Off, true) => w.bytes(b"\x1b[1\"q"),
    }

    // DECSCUSR: cursor shape + blink (blink only from global mode when `emit_global`).
    let blink = emit_global && term.mode(Mode::CursorBlinking);
    emit_decscusr(w, screen.cursor.shape, blink);

    // Terminal-global flags, modes, and margins.
    if emit_global {
        let flags = term.flags();
        if flags.modify_other_keys_2 {
            w.bytes(b"\x1b[>4;2m");
        }
        match flags.mouse_shift_capture {
            Some(true) => w.bytes(b"\x1b[>1s"),
            Some(false) => w.bytes(b"\x1b[>0s"),
            None => {}
        }

        emit_global_modes(w, term);

        let region = term.region();
        if region.top != 0 || region.bottom != rows - 1 {
            w.bytes(b"\x1b[");
            w.dec(region.top + 1);
            w.byte(b';');
            w.dec(region.bottom + 1);
            w.byte(b'r');
            cur_x = 0;
            cur_y = 0;
            cur_pending_wrap = false;
            cursor_moved = true;
        }
        if term.mode(Mode::EnableLeftRightMargin) {
            w.bytes(b"\x1b[?69h");
            if region.left != 0 || region.right != cols - 1 {
                w.bytes(b"\x1b[");
                w.dec(region.left + 1);
                w.byte(b';');
                w.dec(region.right + 1);
                w.byte(b's');
                cur_x = 0;
                cur_y = 0;
                cur_pending_wrap = false;
                cursor_moved = true;
            }
        }
        if term.mode(Mode::Origin) {
            w.bytes(b"\x1b[?6h");
            cur_x = region.left;
            cur_y = region.top;
            cur_pending_wrap = false;
            cursor_moved = true;
        }
    }

    // Final cursor style, hyperlink, and semantic content.
    if !drop_style {
        emit_style_diff(w, cur_style, screen.cursor.style);
    }
    emit_hyperlink(w, screen, &mut cur_link, screen.cursor.hyperlink);
    match screen.cursor.semantic {
        SemanticContent::Output => {}
        SemanticContent::Input => {
            w.bytes(if screen.cursor.semantic_clear_eol {
                b"\x1b]133;I\x1b\\"
            } else {
                b"\x1b]133;B\x1b\\"
            });
        }
        SemanticContent::Prompt => {
            let r = screen.cursor_r();
            let cont = screen.pages.row(r).prompt == RowPrompt::Continuation;
            emit_semantic(w, SemanticContent::Prompt, cont);
        }
    }

    // Final cursor position.
    if !drop_cursor {
        let cx = screen.cursor.x;
        let cy = screen.cursor.y;
        let origin = emit_global && term.mode(Mode::Origin);
        if origin {
            let region = term.region();
            let ry = cy.saturating_sub(region.top);
            let rx = cx.saturating_sub(region.left);
            if rx != 0 || ry != 0 || cur_x != cx || cur_y != cy || cur_pending_wrap {
                emit_cup(w, ry, rx);
            }
        } else if cursor_moved || cx != 0 || cy != 0 || cur_pending_wrap {
            if cur_x != cx || cur_y != cy || cur_pending_wrap {
                emit_cup(w, cy, cx);
            }
        }
    }
}

fn move_x<A: Allocator>(
    w: &mut Writer<A>,
    cur_x: &mut usize,
    cur_pending_wrap: &mut bool,
    target_x: usize,
) {
    if *cur_x == target_x && !*cur_pending_wrap {
        return;
    }
    if target_x == 0 {
        w.byte(b'\r');
    } else if target_x > *cur_x && !*cur_pending_wrap {
        let dx = target_x - *cur_x;
        if dx == 1 {
            w.bytes(b"\x1b[C");
        } else {
            w.bytes(b"\x1b[");
            w.dec(dx);
            w.byte(b'C');
        }
    } else {
        emit_cha(w, target_x);
    }
    *cur_x = target_x;
    *cur_pending_wrap = false;
}

fn emit_cha<A: Allocator>(w: &mut Writer<A>, x: usize) {
    if x == 0 {
        w.bytes(b"\x1b[G");
    } else {
        w.bytes(b"\x1b[");
        w.dec(x + 1);
        w.byte(b'G');
    }
}

fn emit_cup<A: Allocator>(w: &mut Writer<A>, y: usize, x: usize) {
    if y == 0 && x == 0 {
        w.bytes(b"\x1b[H");
    } else {
        w.bytes(b"\x1b[");
        w.dec(y + 1);
        w.byte(b';');
        w.dec(x + 1);
        w.byte(b'H');
    }
}

fn emit_semantic<A: Allocator>(w: &mut Writer<A>, sem: SemanticContent, continuation: bool) {
    match sem {
        SemanticContent::Output => w.bytes(b"\x1b]133;D\x1b\\"),
        SemanticContent::Input => w.bytes(b"\x1b]133;B\x1b\\"),
        SemanticContent::Prompt => {
            w.bytes(if continuation { b"\x1b]133;P;k=c\x1b\\" } else { b"\x1b]133;P;k=i\x1b\\" });
        }
    }
}

fn emit_hyperlink<A: Allocator, P: Planted<VtPlant>>(
    w: &mut Writer<A>,
    screen: &Screen<'_, P>,
    cur_link: &mut u32,
    target_link: u32,
) {
    if *cur_link == target_link {
        return;
    }
    if target_link == 0 {
        w.bytes(b"\x1b]8;;\x1b\\");
    } else if let Some((uri, lid)) = screen.links.get(target_link) {
        w.bytes(b"\x1b]8;");
        if let LinkId::Explicit(id) = lid {
            w.bytes(b"id=");
            w.bytes(id);
        }
        w.byte(b';');
        w.bytes(uri);
        w.bytes(b"\x1b\\");
    } else {
        w.bytes(b"\x1b]8;;\x1b\\");
    }
    *cur_link = target_link;
}

fn emit_decscusr<A: Allocator>(w: &mut Writer<A>, shape: CursorShape, blink: bool) {
    let code: u8 = match (shape, blink) {
        (CursorShape::Block | CursorShape::BlockHollow, false) => return,
        (CursorShape::Block | CursorShape::BlockHollow, true) => b'1',
        (CursorShape::Underline, true) => b'3',
        (CursorShape::Underline, false) => b'4',
        (CursorShape::Bar, true) => b'5',
        (CursorShape::Bar, false) => b'6',
    };
    w.bytes(&[0x1B, b'[', code, b' ', b'q']);
}

fn has_custom_tabstops<P: Planted<VtPlant>>(term: &Terminal<'_, P>) -> bool {
    let cols = term.cols();
    for x in 0..cols {
        let def = x > 0 && x % 8 == 0 && x < cols - 1;
        if term.is_tabstop(x) != def {
            return true;
        }
    }
    false
}

fn emit_charsets<A: Allocator>(
    w: &mut Writer<A>,
    cur: &mut crate::screen::Charsets,
    target: crate::screen::Charsets,
) {
    if *cur == target {
        return;
    }
    let slots = [b'(', b')', b'*', b'+'];
    for (i, &inter) in slots.iter().enumerate() {
        if cur.sets[i] != target.sets[i] {
            let fin = match target.sets[i] {
                Charset::Utf8 | Charset::Ascii => b'B',
                Charset::British => b'A',
                Charset::DecSpecial => b'0',
            };
            w.bytes(&[0x1B, inter, fin]);
            cur.sets[i] = target.sets[i];
        }
    }
    if cur.gl != target.gl {
        match target.gl {
            Slot::G0 => w.byte(0x0F),
            Slot::G1 => w.byte(0x0E),
            Slot::G2 => w.bytes(b"\x1bn"),
            Slot::G3 => w.bytes(b"\x1bo"),
        }
        cur.gl = target.gl;
    }
    if cur.gr != target.gr {
        match target.gr {
            Slot::G0 => {}
            Slot::G1 => w.bytes(b"\x1b~"),
            Slot::G2 => w.bytes(b"\x1b}"),
            Slot::G3 => w.bytes(b"\x1b|"),
        }
        cur.gr = target.gr;
    }
    if cur.single_shift != target.single_shift {
        match target.single_shift {
            Some(Slot::G2) => w.bytes(b"\x1bN"),
            Some(Slot::G3) => w.bytes(b"\x1bO"),
            _ => {}
        }
        cur.single_shift = target.single_shift;
    }
}

fn emit_mode<A: Allocator>(w: &mut Writer<A>, num: u16, ansi: bool, on: bool) {
    w.bytes(if ansi { b"\x1b[" } else { b"\x1b[?" });
    w.dec(usize::from(num));
    w.byte(if on { b'h' } else { b'l' });
}

fn emit_global_modes<A: Allocator, P: Planted<VtPlant>>(
    w: &mut Writer<A>,
    term: &Terminal<'_, P>,
) {
    let flags = term.flags();
    let winner_mouse_event = match flags.mouse_event {
        MouseEvents::None => None,
        MouseEvents::X10 => Some(Mode::MouseX10),
        MouseEvents::Normal => Some(Mode::MouseNormal),
        MouseEvents::Button => Some(Mode::MouseButton),
        MouseEvents::Any => Some(Mode::MouseAny),
    };
    let winner_mouse_format = match flags.mouse_format {
        MouseFormat::X10 => None,
        MouseFormat::Utf8 => Some(Mode::MouseUtf8),
        MouseFormat::Sgr => Some(Mode::MouseSgr),
        MouseFormat::Urxvt => Some(Mode::MouseUrxvt),
        MouseFormat::SgrPixels => Some(Mode::MouseSgrPixels),
    };

    for (mode, num, ansi, default) in MODES {
        match mode {
            // Handled explicitly elsewhere (margins/origin/DECSCUSR/alt-screen/non-replay).
            Mode::Origin
            | Mode::EnableLeftRightMargin
            | Mode::CursorBlinking
            | Mode::Col132
            | Mode::AltScreenLegacy
            | Mode::AltScreen
            | Mode::AltScreenSaveCursorClear
            | Mode::SaveCursor => continue,
            // Defer winning mouse mode so its flag assignment wins last.
            m if Some(m) == winner_mouse_event || Some(m) == winner_mouse_format => continue,
            _ => {}
        }
        let val = term.mode(mode);
        if val != default {
            emit_mode(w, num, ansi, val);
        }
    }

    if let Some(m) = winner_mouse_event {
        emit_mode(w, m.value(), false, true);
    }
    if let Some(m) = winner_mouse_format {
        emit_mode(w, m.value(), false, true);
    }
}

// ---- SGR diff encoder --------------------------------------------------------------------------

struct SgrBatch<'w, A: Allocator> {
    w: &'w mut Writer<A>,
    params: usize,
    open: bool,
}

impl<'w, A: Allocator> SgrBatch<'w, A> {
    fn new(w: &'w mut Writer<A>) -> Self {
        Self { w, params: 0, open: false }
    }

    fn prepare(&mut self, count: usize) {
        if self.open && self.params + count > MAX_SGR_BATCH_PARAMS {
            self.w.byte(b'm');
            self.open = false;
            self.params = 0;
        }
        if !self.open {
            self.w.bytes(b"\x1b[");
            self.open = true;
        } else {
            self.w.byte(b';');
        }
        self.params += count;
    }

    fn code(&mut self, c: usize) {
        self.prepare(1);
        self.w.dec(c);
    }

    fn underline(&mut self, u: Underline) {
        match u {
            Underline::None => self.code(24),
            Underline::Single => self.code(4),
            Underline::Double => {
                self.prepare(2);
                self.w.bytes(b"4:2");
            }
            Underline::Curly => {
                self.prepare(2);
                self.w.bytes(b"4:3");
            }
            Underline::Dotted => {
                self.prepare(2);
                self.w.bytes(b"4:4");
            }
            Underline::Dashed => {
                self.prepare(2);
                self.w.bytes(b"4:5");
            }
        }
    }

    fn color(&mut self, c: Color, base: usize, bright: usize, ext: usize, reset: usize) {
        match c {
            Color::None => self.code(reset),
            Color::Palette(i) if base != 0 && i < 8 => self.code(base + usize::from(i)),
            Color::Palette(i) if bright != 0 && i < 16 => self.code(bright + usize::from(i - 8)),
            Color::Palette(i) => {
                self.prepare(3);
                self.w.dec(ext);
                self.w.bytes(b";5;");
                self.w.dec(usize::from(i));
            }
            Color::Rgb(Rgb { r, g, b }) => {
                self.prepare(5);
                self.w.dec(ext);
                self.w.bytes(b";2;");
                self.w.dec(usize::from(r));
                self.w.byte(b';');
                self.w.dec(usize::from(g));
                self.w.byte(b';');
                self.w.dec(usize::from(b));
            }
        }
    }

    fn finish(self) {
        if self.open {
            self.w.byte(b'm');
        }
    }
}

fn emit_style_diff<A: Allocator>(w: &mut Writer<A>, mut cur: Style, target: Style) {
    if cur == target {
        return;
    }
    if target.is_default() {
        w.bytes(b"\x1b[0m");
        return;
    }

    let mut batch = SgrBatch::new(w);

    // If many fields turn off while few stay on, starting from SGR 0 is shorter and cleaner.
    let turning_off = usize::from(cur.flags.bold && !target.flags.bold)
        + usize::from(cur.flags.faint && !target.flags.faint)
        + usize::from(cur.flags.italic && !target.flags.italic)
        + usize::from(cur.flags.underline != Underline::None && target.flags.underline == Underline::None)
        + usize::from(cur.flags.blink && !target.flags.blink)
        + usize::from(cur.flags.inverse && !target.flags.inverse)
        + usize::from(cur.flags.invisible && !target.flags.invisible)
        + usize::from(cur.flags.strikethrough && !target.flags.strikethrough)
        + usize::from(cur.flags.overline && !target.flags.overline)
        + usize::from(cur.fg != Color::None && target.fg == Color::None)
        + usize::from(cur.bg != Color::None && target.bg == Color::None)
        + usize::from(cur.underline_color != Color::None && target.underline_color == Color::None);

    let staying_on = usize::from(cur.flags.bold && target.flags.bold)
        + usize::from(cur.flags.faint && target.flags.faint)
        + usize::from(cur.flags.italic && target.flags.italic)
        + usize::from(cur.flags.underline != Underline::None && cur.flags.underline == target.flags.underline)
        + usize::from(cur.flags.blink && target.flags.blink)
        + usize::from(cur.flags.inverse && target.flags.inverse)
        + usize::from(cur.flags.invisible && target.flags.invisible)
        + usize::from(cur.flags.strikethrough && target.flags.strikethrough)
        + usize::from(cur.flags.overline && target.flags.overline)
        + usize::from(cur.fg != Color::None && cur.fg == target.fg)
        + usize::from(cur.bg != Color::None && cur.bg == target.bg)
        + usize::from(cur.underline_color != Color::None && cur.underline_color == target.underline_color);

    if turning_off > staying_on + 1 {
        batch.code(0);
        cur = Style::default();
    }

    // Bold & faint share SGR 22 for reset.
    if (cur.flags.bold && !target.flags.bold) || (cur.flags.faint && !target.flags.faint) {
        batch.code(22);
        if target.flags.bold {
            batch.code(1);
        }
        if target.flags.faint {
            batch.code(2);
        }
    } else {
        if !cur.flags.bold && target.flags.bold {
            batch.code(1);
        }
        if !cur.flags.faint && target.flags.faint {
            batch.code(2);
        }
    }

    if cur.flags.italic != target.flags.italic {
        batch.code(if target.flags.italic { 3 } else { 23 });
    }
    if cur.flags.underline != target.flags.underline {
        batch.underline(target.flags.underline);
    }
    if cur.flags.blink != target.flags.blink {
        batch.code(if target.flags.blink { 5 } else { 25 });
    }
    if cur.flags.inverse != target.flags.inverse {
        batch.code(if target.flags.inverse { 7 } else { 27 });
    }
    if cur.flags.invisible != target.flags.invisible {
        batch.code(if target.flags.invisible { 8 } else { 28 });
    }
    if cur.flags.strikethrough != target.flags.strikethrough {
        batch.code(if target.flags.strikethrough { 9 } else { 29 });
    }
    if cur.flags.overline != target.flags.overline {
        batch.code(if target.flags.overline { 53 } else { 55 });
    }

    if cur.fg != target.fg {
        batch.color(target.fg, 30, 90, 38, 39);
    }
    if cur.bg != target.bg {
        batch.color(target.bg, 40, 100, 48, 49);
    }
    if cur.underline_color != target.underline_color {
        batch.color(target.underline_color, 0, 0, 58, 59);
    }

    batch.finish();
}
