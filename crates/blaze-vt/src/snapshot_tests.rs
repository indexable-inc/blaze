//! Unit tests and `blaze-sim` property tests (with planted-bug controls) for canonical VT
//! snapshot & restore (`snapshot.rs`).

use core::fmt;
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::osc::MouseShape;
use crate::page::{RowPrompt, SemanticContent, Wide};
use crate::plant::VtPlant;
use crate::screen::{CursorShape, LinkId, Screen};
use crate::snapshot::{RestoreError, VtSnapshot, capture, restore};
use crate::style::{Color, Rgb, Style, Underline};
use crate::terminal::modes::MODES;
use crate::terminal::{Mode, Terminal, Which};
use crate::test_support::{all_text, heap, history_text, opts, term, text};

// ---- full terminal equivalence oracle ----------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
enum SnapBroken {
    Dimensions { want: (usize, usize), got: (usize, usize) },
    ActiveScreen { want: Which, got: Which },
    Rows { screen: &'static str, want: (usize, usize), got: (usize, usize) },
    Cell {
        screen: &'static str,
        row: usize,
        x: usize,
        want: CellView,
        got: CellView,
    },
    Wrap {
        screen: &'static str,
        row: usize,
        want: (bool, bool, RowPrompt),
        got: (bool, bool, RowPrompt),
    },
    Cursor {
        screen: &'static str,
        want: CursorView,
        got: CursorView,
    },
    Mode { mode: Mode, want: bool, got: bool },
    Region { want: crate::terminal::Region, got: crate::terminal::Region },
    Flags { want: crate::terminal::Flags, got: crate::terminal::Flags },
    Title { want: StdVec<u8>, got: StdVec<u8> },
    Pwd { want: StdVec<u8>, got: StdVec<u8> },
    Colors,
    StyleRefs { screen: &'static str, id: u16, refs: u32, named: u32 },
    Resized { row: usize, x: usize, want: CellView, got: CellView },
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct CellView {
    cp: u32,
    wide: Wide,
    style: Style,
    graphemes: StdVec<u32>,
    link_uri: Option<StdVec<u8>>,
    protected: bool,
    is_bg: bool,
    semantic: SemanticContent,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct CursorView {
    x: usize,
    y: usize,
    pending_wrap: bool,
    shape: CursorShape,
    style: Style,
    protected: bool,
    link_uri: Option<StdVec<u8>>,
}

impl fmt::Display for SnapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

fn link_uri<P: Planted<VtPlant>>(s: &Screen<'_, P>, id: u32) -> Option<StdVec<u8>> {
    if id == 0 {
        return None;
    }
    s.links.get(id).map(|(uri, _)| uri.to_vec())
}

fn cell_view<P: Planted<VtPlant>>(s: &Screen<'_, P>, r: usize, x: usize) -> CellView {
    let c = s.pages.cell(r, x);
    // An empty cell (`cp == 0, Narrow, !is_bg`) or a `SpacerHead` draws nothing (no glyph, no
    // background) and may retain unobservable stale `style_id`/`hyperlink` from `unwide` or
    // `row_will_be_shifted`; every visible cell (`has_text`, `is_bg`, `SpacerTail`) compares
    // its full `Style` and hyperlink URI byte-for-byte.
    let unobservable = c.is_empty() || c.wide() == Wide::SpacerHead;
    let style = if unobservable { Style::default() } else { *s.style(c.style_id()) };
    let link = if unobservable { None } else { link_uri(s, s.cell_link(c)) };
    CellView {
        cp: c.codepoint(),
        wide: c.wide(),
        style,
        graphemes: s.graphemes(c).to_vec(),
        link_uri: link,
        protected: c.protected(),
        is_bg: c.is_bg(),
        semantic: c.semantic(),
    }
}

fn cursor_view<P: Planted<VtPlant>>(s: &Screen<'_, P>) -> CursorView {
    let c = s.cursor;
    CursorView {
        x: c.x,
        y: c.y,
        pending_wrap: c.pending_wrap,
        shape: c.shape,
        style: c.style,
        protected: c.protected,
        link_uri: link_uri(s, c.hyperlink),
    }
}

fn check_style_refs<P: Planted<VtPlant>>(
    name: &'static str,
    s: &Screen<'_, P>,
) -> Result<(), Broken<SnapBroken>> {
    let mut named: BTreeMap<u16, u32> = BTreeMap::new();
    for r in 0..s.pages.total() {
        for c in s.pages.cells(r) {
            if c.style_id() != 0 {
                *named.entry(c.style_id()).or_default() += 1;
            }
        }
    }
    if s.cursor.style_id != 0 {
        *named.entry(s.cursor.style_id).or_default() += 1;
    }
    for (&id, &n) in &named {
        let refs = s.styles.refs(id);
        if refs != n {
            return Err(Broken::Oracle(SnapBroken::StyleRefs {
                screen: name,
                id,
                refs,
                named: n,
            }));
        }
    }
    if s.styles.live() != named.len() {
        return Err(Broken::Harness(std::format!(
            "{name}: {} styles live but {} named: style leak",
            s.styles.live(),
            named.len()
        )));
    }
    Ok(())
}

fn compare_screens<P: Planted<VtPlant>>(
    name: &'static str,
    a: &Screen<'_, P>,
    b: &Screen<'_, P>,
) -> Result<(), Broken<SnapBroken>> {
    if (a.pages.total(), a.pages.scrollback()) != (b.pages.total(), b.pages.scrollback()) {
        return Err(Broken::Oracle(SnapBroken::Rows {
            screen: name,
            want: (a.pages.total(), a.pages.scrollback()),
            got: (b.pages.total(), b.pages.scrollback()),
        }));
    }
    for r in 0..a.pages.total() {
        let ra = a.pages.row(r);
        let rb = b.pages.row(r);
        if (ra.wrap, ra.wrap_continuation, ra.prompt) != (rb.wrap, rb.wrap_continuation, rb.prompt)
        {
            return Err(Broken::Oracle(SnapBroken::Wrap {
                screen: name,
                row: r,
                want: (ra.wrap, ra.wrap_continuation, ra.prompt),
                got: (rb.wrap, rb.wrap_continuation, rb.prompt),
            }));
        }
        for x in 0..a.cols() {
            let va = cell_view(a, r, x);
            let vb = cell_view(b, r, x);
            if va != vb {
                return Err(Broken::Oracle(SnapBroken::Cell {
                    screen: name,
                    row: r,
                    x,
                    want: va,
                    got: vb,
                }));
            }
        }
    }
    let ca = cursor_view(a);
    let cb = cursor_view(b);
    if ca != cb {
        return Err(Broken::Oracle(SnapBroken::Cursor { screen: name, want: ca, got: cb }));
    }
    check_style_refs(name, b)
}

fn compare_terminals<P: Planted<VtPlant>>(
    a: &Terminal<'_, P>,
    b: &Terminal<'_, P>,
) -> Result<(), Broken<SnapBroken>> {
    if (a.cols(), a.rows()) != (b.cols(), b.rows()) {
        return Err(Broken::Oracle(SnapBroken::Dimensions {
            want: (a.cols(), a.rows()),
            got: (b.cols(), b.rows()),
        }));
    }
    if a.active() != b.active() {
        return Err(Broken::Oracle(SnapBroken::ActiveScreen {
            want: a.active(),
            got: b.active(),
        }));
    }
    compare_screens("primary", a.primary(), b.primary())?;
    if let (Some(aa), Some(ba)) = (a.alternate(), b.alternate()) {
        compare_screens("alternate", aa, ba)?;
    } else if a.active() == Which::Alternate {
        return Err(Broken::Oracle(SnapBroken::ActiveScreen {
            want: Which::Alternate,
            got: Which::Primary,
        }));
    }
    for (mode, _, _, _) in MODES {
        if a.mode(mode) != b.mode(mode) {
            return Err(Broken::Oracle(SnapBroken::Mode {
                mode,
                want: a.mode(mode),
                got: b.mode(mode),
            }));
        }
    }
    if a.region() != b.region() {
        return Err(Broken::Oracle(SnapBroken::Region { want: a.region(), got: b.region() }));
    }
    if *a.flags() != *b.flags() {
        return Err(Broken::Oracle(SnapBroken::Flags { want: *a.flags(), got: *b.flags() }));
    }
    if a.title() != b.title() {
        return Err(Broken::Oracle(SnapBroken::Title {
            want: a.title().to_vec(),
            got: b.title().to_vec(),
        }));
    }
    if a.pwd() != b.pwd() {
        return Err(Broken::Oracle(SnapBroken::Pwd {
            want: a.pwd().to_vec(),
            got: b.pwd().to_vec(),
        }));
    }
    if a.colors() != b.colors() {
        return Err(Broken::Oracle(SnapBroken::Colors));
    }
    Ok(())
}

fn round_trip(orig: &Terminal<'static>) -> (VtSnapshot<blaze_own::Alloc<'static>>, Terminal<'static>) {
    let snap = orig.snapshot_vt(heap()).expect("snapshot alloc");
    let mut restored = term(orig.cols() as u16, orig.rows() as u16);
    restored.restore_vt_snapshot(&snap).expect("restore");
    compare_terminals(orig, &restored).expect("restored terminal matches original");
    (snap, restored)
}

// ---- unit tests --------------------------------------------------------------------------------

#[test]
fn snapshot_blank_terminal_is_minimal() {
    let t = term(80, 24);
    let (snap, _restored) = round_trip(&t);
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
    assert!(!snap.active_alt);
    assert!(snap.primary_vt.is_empty(), "blank terminal should emit 0 VT bytes");
    assert!(snap.alt_vt.is_none());
}

#[test]
fn snapshot_styled_text_rgb_256_and_all_flags() {
    let mut t = term(40, 6);
    // Truecolor RGB fg + bg + curly colored underline + bold + italic + overline + strikethrough
    t.feed(
        b"\x1b[1;3;4:3;9;53;38;2;255;128;64;48;2;10;20;30;58;2;0;200;100mRGB\x1b[0m\r\n\
          \x1b[2;5;7;8;4:4;38;5;196;48;5;235;58;5;82mPAL256\x1b[0m\r\n\
          \x1b[4:2;31;44mDBL\x1b[4:5;92;103mDSH\x1b[4;39;49mSGL\x1b[0m\r\n\
          \x1b[48;2;40;50;60m\x1b[5X\x1b[5Cafter",
    );
    let (_snap, restored) = round_trip(&t);

    let s = restored.screen();
    let st0 = *s.style(s.pages.cell(s.r(0), 0).style_id());
    assert_eq!(st0.fg, Color::Rgb(Rgb { r: 255, g: 128, b: 64 }));
    assert_eq!(st0.bg, Color::Rgb(Rgb { r: 10, g: 20, b: 30 }));
    assert_eq!(st0.underline_color, Color::Rgb(Rgb { r: 0, g: 200, b: 100 }));
    assert!(st0.flags.bold && st0.flags.italic && st0.flags.strikethrough && st0.flags.overline);
    assert_eq!(st0.flags.underline, Underline::Curly);

    let st1 = *s.style(s.pages.cell(s.r(1), 0).style_id());
    assert_eq!(st1.fg, Color::Palette(196));
    assert_eq!(st1.bg, Color::Palette(235));
    assert_eq!(st1.underline_color, Color::Palette(82));
    assert!(st1.flags.faint && st1.flags.blink && st1.flags.inverse && st1.flags.invisible);
    assert_eq!(st1.flags.underline, Underline::Dotted);

    // Background-only cells from ECH preserved as `is_bg()` cells.
    let bg_cell = s.pages.cell(s.r(3), 2);
    assert!(bg_cell.is_bg());
    assert_eq!(s.style(bg_cell.style_id()).bg, Color::Rgb(Rgb { r: 40, g: 50, b: 60 }));
}

#[test]
fn snapshot_wide_cjk_spacer_head_and_combining_graphemes() {
    let mut t = term(5, 4);
    // Row 0: "abc" (cols 0..3), then '漢' doesn't fit at col 4? Wait: cols=5: "abcd" (0..4), '漢'
    // wraps leaving SpacerHead at col 4, then '漢' at (1, 0..2), then combining e + U+0301,
    // then wide '字' + combining U+0308.
    t.feed("abcd漢字e\u{0301}字\u{0308}".as_bytes());
    // Also test Mode 2027 ZWJ emoji + FE0F widening on row 3.
    t.feed("\r\n\x1b[?2027h\u{2764}\u{FE0F}ok\x1b[?2027l".as_bytes());

    let (_snap, restored) = round_trip(&t);
    assert_eq!(text(&restored), text(&t));

    // Verify SpacerHead at (0, 4) and Wide at (1, 0).
    let s = restored.screen();
    assert_eq!(s.pages.cell(s.r(0), 4).wide(), Wide::SpacerHead);
    assert!(s.pages.row(s.r(0)).wrap);
    assert!(s.pages.row(s.r(1)).wrap_continuation);
    assert_eq!(s.pages.cell(s.r(1), 0).wide(), Wide::Wide);
    assert_eq!(s.pages.cell(s.r(1), 0).codepoint(), u32::from('漢'));
}

#[test]
fn snapshot_scrollback_and_resize_reflow() {
    let mut t = term(6, 3);
    // Write enough lines to create multi-page scrollback (TEST_PAGE_ROWS = 3) with soft wraps.
    t.feed(b"line01\r\nline02\r\nlongwrappedline\r\nline04\r\nline05\r\ntail");
    assert!(t.screen().pages.scrollback() >= 4);

    let (snap, mut restored) = round_trip(&t);
    assert_eq!(history_text(&restored), history_text(&t));
    assert_eq!(all_text(&restored), all_text(&t));

    // Verify bit-packed soft-wrap metadata matches.
    for r in 0..t.primary().pages.total() {
        let row = t.primary().pages.row(r);
        assert_eq!(snap.primary_row_wrap(r), row.wrap);
        assert_eq!(snap.primary_row_wrap_continuation(r), row.wrap_continuation);
    }

    // Resize both terminals wider and verify identical reflowed output!
    let mut orig_resized = t;
    orig_resized.resize(20, 4);
    restored.resize(20, 4);
    assert_eq!(all_text(&restored), all_text(&orig_resized));
    compare_terminals(&orig_resized, &restored).expect("post-resize reflow matches");
}

#[test]
fn snapshot_cursor_position_shape_and_modes() {
    let mut t = term(20, 8);
    // Set scroll region 2..7, enable left-right margins 3..18, enable origin mode,
    // disable autowrap, hide cursor, enable bracketed paste, focus reporting, SGR mouse,
    // blinking bar cursor, move cursor inside margins, set pen style.
    t.feed(
        b"header\r\nbody\x1b[2;7r\x1b[?69h\x1b[3;18s\x1b[?6h\x1b[?7l\x1b[?25l\
          \x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[5 q\x1b[3;4H\x1b[1;33m",
    );

    let (_snap, mut restored) = round_trip(&t);
    assert_eq!(restored.screen().cursor.shape, CursorShape::Bar);
    assert!(restored.mode(Mode::CursorBlinking));
    assert!(!restored.mode(Mode::Wraparound));
    assert!(restored.mode(Mode::Origin));
    assert!(!restored.mode(Mode::CursorVisible));
    assert!(restored.mode(Mode::BracketedPaste));
    assert!(restored.mode(Mode::FocusEvent));
    assert!(restored.mode(Mode::MouseNormal));
    assert!(restored.mode(Mode::MouseSgr));
    assert_eq!(restored.screen().cursor.x, 5); // left(2) + (4 - 1)
    assert_eq!(restored.screen().cursor.y, 3); // top(1) + (3 - 1)
    // Restore must not leave spurious focus/size replies or effects queued.
    assert!(restored.take_replies().is_empty());
    assert!(restored.take_effects().is_empty());
}

#[test]
fn snapshot_osc_title_cwd_palette_mouse_shape_and_prompts() {
    let mut t = term(30, 5);
    t.feed(
        b"\x1b]2;blaze vt session\x1b\\\
          \x1b]7;file://localhost/Users/dev/my%20repo%3F1%23a%25b\x1b\\\
          \x1b]4;1;rgb:aa/bb/cc\x1b\\\
          \x1b]4;200;rgb:12/34/56\x1b\\\
          \x1b]10;rgb:ee/ee/ee\x1b\\\
          \x1b]11;rgb:11/22/33\x1b\\\
          \x1b]12;rgb:ff/00/88\x1b\\\
          \x1b]22;pointer\x1b\\\
          \x1b]133;A\x1b\\$ \x1b]133;B\x1b\\ls -la\x1b]133;C\x1b\\\r\noutput\x1b]133;D;0\x1b\\",
    );

    let (_snap, restored) = round_trip(&t);
    assert_eq!(restored.title(), b"blaze vt session");
    assert_eq!(restored.pwd(), b"/Users/dev/my repo?1#a%b");
    assert_eq!(restored.colors().palette.current[1], Rgb { r: 0xaa, g: 0xbb, b: 0xcc });
    assert_eq!(restored.colors().palette.current[200], Rgb { r: 0x12, g: 0x34, b: 0x56 });
    assert_eq!(restored.colors().foreground.override_, Some(Rgb { r: 0xee, g: 0xee, b: 0xee }));
    assert_eq!(restored.colors().background.override_, Some(Rgb { r: 0x11, g: 0x22, b: 0x33 }));
    assert_eq!(restored.colors().cursor.override_, Some(Rgb { r: 0xff, g: 0x00, b: 0x88 }));
    assert_eq!(restored.mouse_shape(), MouseShape::Pointer);
    assert_eq!(restored.primary().pages.row(restored.primary().r(0)).prompt, RowPrompt::Prompt);
}

#[test]
fn snapshot_osc8_hyperlinks() {
    let mut t = term(30, 4);
    t.feed(
        b"\x1b]8;id=doc42;https://blaze.dev/vt\x1b\\click\x1b]8;;\x1b\\ \
          \x1b]8;;https://example.com\x1b\\open",
    );

    let (_snap, restored) = round_trip(&t);
    let s = restored.screen();
    let link_explicit = s.cell_link(s.pages.cell(s.r(0), 0));
    assert_ne!(link_explicit, 0);
    assert_eq!(
        s.links.get(link_explicit),
        Some((&b"https://blaze.dev/vt"[..], LinkId::Explicit(b"doc42")))
    );
    assert_eq!(s.cell_link(s.pages.cell(s.r(0), 5)), 0);
    let link_open = s.cell_link(s.pages.cell(s.r(0), 6));
    assert_ne!(link_open, 0);
    assert_eq!(s.links.get(link_open).map(|(u, _)| u), Some(&b"https://example.com"[..]));
    // Cursor's open hyperlink also survives.
    assert_ne!(s.cursor.hyperlink, 0);
    assert_eq!(
        s.links.get(s.cursor.hyperlink).map(|(u, _)| u),
        Some(&b"https://example.com"[..])
    );
}

#[test]
fn snapshot_alt_screen_active_and_dormant_round_trip() {
    let mut t = term(16, 4);
    // Populate primary with scrollback + visible rows + title.
    t.feed(b"\x1b]2;vim\x1b\\s1\r\ns2\r\ns3\r\ns4\r\ns5\r\nprompt$ ");
    // Enter alt screen via ?1049h (saves primary cursor, clears alt) and draw TUI.
    t.feed(b"\x1b[?1049h\x1b[1;1H\x1b[?25l\x1b[1;36mALT TOP\x1b[4;1H-- INSERT --\x1b[2;5H");

    let (snap, mut restored) = round_trip(&t);
    assert!(snap.active_alt);
    assert!(snap.alt_vt.is_some());
    assert_eq!(restored.active(), Which::Alternate);
    assert_eq!(text(&restored), "ALT TOP\n\n\n-- INSERT --");

    // Exiting alt screen (?1049l) must return to the exact primary screen & saved cursor!
    t.feed(b"\x1b[?1049l");
    restored.feed(b"\x1b[?1049l");
    assert_eq!(restored.active(), Which::Primary);
    assert_eq!(all_text(&restored), all_text(&t));
    assert_eq!(restored.screen().cursor.x, t.screen().cursor.x);
    assert_eq!(restored.screen().cursor.y, t.screen().cursor.y);

    // Dormant non-empty alt screen (?47h ... ?47l) also round-trips.
    let mut t2 = term(10, 3);
    t2.feed(b"primary\x1b[?47h\x1b[1;1Halt-stay\x1b[?47l");
    let (snap2, mut restored2) = round_trip(&t2);
    assert!(!snap2.active_alt);
    assert!(snap2.alt_vt.is_some());
    t2.feed(b"\x1b[?47h");
    restored2.feed(b"\x1b[?47h");
    assert_eq!(text(&restored2), "alt-stay");
}

#[test]
fn snapshot_restore_refuses_dimension_mismatch() {
    let mut t = term(20, 6);
    t.feed(b"hello");
    let snap = t.snapshot_vt(heap()).unwrap();
    let mut wrong = term(10, 6);
    assert_eq!(
        wrong.restore_vt_snapshot(&snap),
        Err(RestoreError::DimensionMismatch {
            snapshot_cols: 20,
            snapshot_rows: 6,
            terminal_cols: 10,
            terminal_rows: 6,
        })
    );
}

#[test]
fn snapshot_is_deterministic_and_idempotent() {
    let mut t = term(16, 4);
    t.feed(
        b"\x1b]2;idemp\x1b\\\x1b[38;2;1;2;3mhi \xE6\xBC\xA2\x1b[0m\r\n\
          \x1b]8;id=k;https://a.b\x1b\\link\x1b]8;;\x1b\\\r\nline3\r\nline4\r\nline5",
    );
    let snap1 = t.snapshot_vt(heap()).unwrap();
    let snap2 = t.snapshot_vt(heap()).unwrap();
    assert_eq!(snap1, snap2);

    let mut r = term(16, 4);
    r.restore_vt_snapshot(&snap1).unwrap();
    let snap3 = r.snapshot_vt(heap()).unwrap();
    assert_eq!(snap1, snap3);
}

// ---- blaze-sim property test & planted-bug controls --------------------------------------------

#[derive(Clone, Debug)]
enum SnapOp {
    PrintAscii(String),
    PrintWide,
    PrintCombining,
    Cr,
    Lf,
    Bs,
    Cup(u16, u16),
    Cuu(u16),
    Cud(u16),
    Cuf(u16),
    Cub(u16),
    Su(u16),
    Sd(u16),
    Il(u16),
    Dl(u16),
    Ich(u16),
    Dch(u16),
    Ech(u16),
    El(u8),
    Ed(u8),
    Stbm(u16, u16),
    SgrReset,
    SgrFlag(u8),
    SgrUnderline(u8),
    SgrFgPal(u8),
    SgrBgPal(u8),
    SgrFgRgb(u8, u8, u8),
    SgrBgRgb(u8, u8, u8),
    DecMode(u16, bool),
    CursorShape(u8),
    HyperlinkStart(u8),
    HyperlinkEnd,
    Title(u8),
    Pwd(u8),
    Palette(u8, u8, u8, u8),
    AltScreen(bool),
}

impl SnapOp {
    fn draw(tape: &Tape, cols: u16, rows: u16) -> SnapOp {
        match tape.below(34, "op") {
            0..=5 => {
                let len = tape.int_in(1..=10usize, "len");
                let s = (0..len)
                    .map(|_| *tape.pick(&['a', 'b', 'Z', '0', ' ', '#'], "ch").unwrap())
                    .collect();
                SnapOp::PrintAscii(s)
            }
            6..=7 => SnapOp::PrintWide,
            8 => SnapOp::PrintCombining,
            9 => SnapOp::Cr,
            10 => SnapOp::Lf,
            11 => SnapOp::Bs,
            12..=13 => {
                SnapOp::Cup(tape.int_in(1..=rows + 1, "r"), tape.int_in(1..=cols + 1, "c"))
            }
            14 => match tape.below(4, "dir") {
                0 => SnapOp::Cuu(tape.int_in(1..=rows, "n")),
                1 => SnapOp::Cud(tape.int_in(1..=rows, "n")),
                2 => SnapOp::Cuf(tape.int_in(1..=cols, "n")),
                _ => SnapOp::Cub(tape.int_in(1..=cols, "n")),
            },
            15 => SnapOp::Su(tape.int_in(1..=rows, "n")),
            16 => SnapOp::Sd(tape.int_in(1..=rows, "n")),
            17 => SnapOp::Il(tape.int_in(1..=rows, "n")),
            18 => SnapOp::Dl(tape.int_in(1..=rows, "n")),
            19 => SnapOp::Ich(tape.int_in(1..=cols, "n")),
            20 => SnapOp::Dch(tape.int_in(1..=cols, "n")),
            21 => SnapOp::Ech(tape.int_in(1..=cols, "n")),
            22 => SnapOp::El(tape.int_in(0..=2, "m")),
            23 => SnapOp::Ed(tape.int_in(0..=3, "m")),
            24 => {
                let top = tape.int_in(1..=rows - 1, "top");
                SnapOp::Stbm(top, tape.int_in(top + 1..=rows, "bot"))
            }
            25 => SnapOp::SgrReset,
            26 => SnapOp::SgrFlag(*tape.pick(&[1u8, 2, 3, 5, 7, 9, 22, 23, 27, 53, 55], "f").unwrap()),
            27 => SnapOp::SgrUnderline(tape.int_in(0..=5, "ul")),
            28 => {
                if tape.coin("bg") {
                    SnapOp::SgrBgPal(tape.int_in(0..=255, "idx"))
                } else {
                    SnapOp::SgrFgPal(tape.int_in(0..=255, "idx"))
                }
            }
            29 => {
                let r = tape.int_in(0..=255, "r");
                let g = tape.int_in(0..=255, "g");
                let b = tape.int_in(0..=255, "b");
                if tape.coin("bg") { SnapOp::SgrBgRgb(r, g, b) } else { SnapOp::SgrFgRgb(r, g, b) }
            }
            30 => {
                let m = *tape.pick(&[6u16, 7, 25, 1000, 1004, 1006, 2004], "mode").unwrap();
                SnapOp::DecMode(m, tape.coin("on"))
            }
            31 => SnapOp::CursorShape(tape.int_in(1..=6, "shape")),
            32 => match tape.below(4, "osc") {
                0 => SnapOp::HyperlinkStart(tape.int_in(0..=3, "id")),
                1 => SnapOp::HyperlinkEnd,
                2 => SnapOp::Title(tape.int_in(0..=3, "t")),
                _ => SnapOp::Pwd(tape.int_in(0..=3, "p")),
            },
            _ => {
                if tape.coin("alt") {
                    SnapOp::AltScreen(tape.coin("enter"))
                } else {
                    SnapOp::Palette(
                        tape.int_in(0..=15, "i"),
                        tape.int_in(0..=255, "r"),
                        tape.int_in(0..=255, "g"),
                        tape.int_in(0..=255, "b"),
                    )
                }
            }
        }
    }

    fn write_bytes(&self, out: &mut StdVec<u8>) {
        use std::io::Write as _;
        let _ = match self {
            SnapOp::PrintAscii(s) => write!(out, "{s}"),
            SnapOp::PrintWide => out.write_all("漢".as_bytes()),
            SnapOp::PrintCombining => out.write_all("e\u{0301}".as_bytes()),
            SnapOp::Cr => out.write_all(b"\r"),
            SnapOp::Lf => out.write_all(b"\n"),
            SnapOp::Bs => out.write_all(b"\x08"),
            SnapOp::Cup(r, c) => write!(out, "\x1b[{r};{c}H"),
            SnapOp::Cuu(n) => write!(out, "\x1b[{n}A"),
            SnapOp::Cud(n) => write!(out, "\x1b[{n}B"),
            SnapOp::Cuf(n) => write!(out, "\x1b[{n}C"),
            SnapOp::Cub(n) => write!(out, "\x1b[{n}D"),
            SnapOp::Su(n) => write!(out, "\x1b[{n}S"),
            SnapOp::Sd(n) => write!(out, "\x1b[{n}T"),
            SnapOp::Il(n) => write!(out, "\x1b[{n}L"),
            SnapOp::Dl(n) => write!(out, "\x1b[{n}M"),
            SnapOp::Ich(n) => write!(out, "\x1b[{n}@"),
            SnapOp::Dch(n) => write!(out, "\x1b[{n}P"),
            SnapOp::Ech(n) => write!(out, "\x1b[{n}X"),
            SnapOp::El(m) => write!(out, "\x1b[{m}K"),
            SnapOp::Ed(m) => write!(out, "\x1b[{m}J"),
            SnapOp::Stbm(t, b) => write!(out, "\x1b[{t};{b}r"),
            SnapOp::SgrReset => out.write_all(b"\x1b[0m"),
            SnapOp::SgrFlag(f) => write!(out, "\x1b[{f}m"),
            SnapOp::SgrUnderline(u) => write!(out, "\x1b[4:{u}m"),
            SnapOp::SgrFgPal(i) => write!(out, "\x1b[38;5;{i}m"),
            SnapOp::SgrBgPal(i) => write!(out, "\x1b[48;5;{i}m"),
            SnapOp::SgrFgRgb(r, g, b) => write!(out, "\x1b[38;2;{r};{g};{b}m"),
            SnapOp::SgrBgRgb(r, g, b) => write!(out, "\x1b[48;2;{r};{g};{b}m"),
            SnapOp::DecMode(m, on) => write!(out, "\x1b[?{m}{}", if *on { 'h' } else { 'l' }),
            SnapOp::CursorShape(s) => write!(out, "\x1b[{s} q"),
            SnapOp::HyperlinkStart(id) => write!(out, "\x1b]8;id=k{id};https://ex/{id}\x1b\\"),
            SnapOp::HyperlinkEnd => out.write_all(b"\x1b]8;;\x1b\\"),
            SnapOp::Title(t) => write!(out, "\x1b]2;title-{t}\x1b\\"),
            SnapOp::Pwd(p) => write!(out, "\x1b]7;file://localhost/dir/{p}\x1b\\"),
            SnapOp::Palette(i, r, g, b) => {
                write!(out, "\x1b]4;{i};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\")
            }
            SnapOp::AltScreen(enter) => write!(out, "\x1b[?1049{}", if *enter { 'h' } else { 'l' }),
        };
    }
}

fn snapshot_round_trip_property(
    tape: &Tape,
    plant: impl Planted<VtPlant>,
) -> Result<(), Broken<SnapBroken>> {
    let cols = tape.int_in(2..=10u16, "cols");
    let rows = tape.int_in(2..=7u16, "rows");
    let ops = tape.vec(1..=45, "ops", |t| SnapOp::draw(t, cols, rows));

    let mut orig = Terminal::new_planted(opts(cols, rows), heap(), plant)
        .map_err(|e| Broken::Harness(std::format!("heap refused: {e:?}")))?;
    let mut stream = StdVec::new();
    for op in &ops {
        stream.clear();
        op.write_bytes(&mut stream);
        orig.feed(&stream);
    }

    let snap = capture(&orig, heap())
        .map_err(|e| Broken::Harness(std::format!("snapshot refused: {e:?}")))?;
    let mut restored = Terminal::new_planted(opts(cols, rows), heap(), plant)
        .map_err(|e| Broken::Harness(std::format!("heap refused: {e:?}")))?;
    restore(&mut restored, &snap)
        .map_err(|e| Broken::Harness(std::format!("restore failed: {e}")))?;

    compare_terminals(&orig, &restored)?;

    // Also verify post-restore resize reflow matches identically!
    if tape.coin("resize_after_restore") {
        let to_cols = tape.int_in(2..=12u16, "to_cols");
        let to_rows = tape.int_in(2..=8u16, "to_rows");
        orig.resize(to_cols, to_rows);
        restored.resize(to_cols, to_rows);
        let a = orig.primary();
        let b = restored.primary();
        if (a.pages.total(), a.pages.scrollback()) != (b.pages.total(), b.pages.scrollback()) {
            return Err(Broken::Oracle(SnapBroken::Rows {
                screen: "primary(resized)",
                want: (a.pages.total(), a.pages.scrollback()),
                got: (b.pages.total(), b.pages.scrollback()),
            }));
        }
        for r in 0..a.pages.total() {
            for x in 0..a.cols() {
                let va = cell_view(a, r, x);
                let vb = cell_view(b, r, x);
                if va != vb {
                    return Err(Broken::Oracle(SnapBroken::Resized {
                        row: r,
                        x,
                        want: va,
                        got: vb,
                    }));
                }
            }
        }
    }

    Ok(())
}

/// The invariant: for any drawn VT stream, `snapshot_vt` -> `restore_vt_snapshot` produces a
/// terminal whose screens, scrollback, styles, graphemes, hyperlinks, soft-wraps, cursor,
/// modes, margins, colors, title, cwd, and subsequent resize reflow match the original.
#[test]
fn snapshot_round_trip_preserves_terminal_state() {
    check(Config::cases(2000), |tape| snapshot_round_trip_property(tape, NoPlant));
}

blaze_sim::plants! {
    /// Planted bugs in snapshot capture, each caught by the property oracle.
    enum SnapPlant breaks SnapBroken {
        /// The code as it is.
        None,
        /// Snapshot omits SGR transitions: styled cells or cursor pen lose their style.
        SnapshotDropsStyle => SnapBroken::Cell { .. } | SnapBroken::Cursor { .. },
        /// Snapshot omits the final cursor position (CUP): cursor lands in the wrong place.
        SnapshotDropsCursor => SnapBroken::Cursor { .. },
        /// Snapshot zeroes primary soft-wrap metadata: row wrap flags or post-resize reflow differ.
        SnapshotDropsSoftWrap => SnapBroken::Wrap { .. } | SnapBroken::Resized { .. },
    }
}

impl SnapPlant {
    fn planted(self) -> VtPlant {
        match self {
            SnapPlant::None => VtPlant::None,
            SnapPlant::SnapshotDropsStyle => VtPlant::SnapshotDropsStyle,
            SnapPlant::SnapshotDropsCursor => VtPlant::SnapshotDropsCursor,
            SnapPlant::SnapshotDropsSoftWrap => VtPlant::SnapshotDropsSoftWrap,
        }
    }
}

#[test]
fn the_oracle_catches_every_snapshot_plant() {
    controls(Config::cases(500), |tape, plant: SnapPlant| {
        snapshot_round_trip_property(tape, plant.planted())
    });
}
