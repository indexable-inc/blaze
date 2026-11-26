//! End to end: bytes through [`Terminal::feed`], checked on the screen, the styles of named
//! cells, the replies and the effects. Ported tests name ghostty's test and its file:line
//! (stream_terminal.zig, Terminal.zig); the rest check one wire each.

use std::vec::Vec as StdVec;

use crate::input::mouse::Pixels;
use crate::kitty::exec::ScreenKey;
use crate::kitty::storage::Location;
use crate::osc::{Clipboard, MouseShape, ProgressReport, ProgressState};
use crate::page::{RowPrompt, SemanticContent};
use crate::screen::PromptRedraw;
use crate::style::{Color, Rgb, Style, Underline};
use crate::terminal::{Effect, Effects, Span, Terminal};
use crate::test_support::{cell, cursor, fed, opts, replies, term, text};

fn style_at(t: &Terminal<'_>, x: usize, y: usize) -> Style {
    *t.screen().style(cell(t, x, y).style_id())
}

/// The effects taken: the list, and the [`Effects`] whose bytes its spans name.
fn effects(t: &mut Terminal<'static>) -> (StdVec<Effect>, Effects<'static>) {
    let e = t.take_effects();
    (e.as_slice().to_vec(), e)
}

fn semantic_at(t: &Terminal<'_>, x: usize, y: usize) -> SemanticContent {
    cell(t, x, y).semantic()
}

fn row_prompt(t: &Terminal<'_>, y: usize) -> RowPrompt {
    let s = t.screen();
    s.pages.row(s.r(y)).prompt
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

// ---- SGR ----------------------------------------------------------------------------------

#[test]
fn sgr_bold_and_colon_rgb() {
    let t = fed(10, 3, b"\x1b[1;38:2::255:0:0mX");
    let st = style_at(&t, 0, 0);
    assert!(st.flags.bold);
    assert_eq!(st.fg, Color::Rgb(rgb(255, 0, 0)));
    assert_eq!(text(&t), "X");
}

#[test]
fn sgr_reset_and_palette() {
    let t = fed(10, 3, b"\x1b[3;4:3;58;5;9;48;5;200mA\x1b[0mB\x1b[91mC");
    let a = style_at(&t, 0, 0);
    assert!(a.flags.italic);
    assert_eq!(a.flags.underline, Underline::Curly);
    assert_eq!(a.underline_color, Color::Palette(9));
    assert_eq!(a.bg, Color::Palette(200));
    assert_eq!(style_at(&t, 1, 0), Style::default());
    assert_eq!(style_at(&t, 2, 0).fg, Color::Palette(9));
}

#[test]
fn attributes() {
    // stream_terminal.zig:1053 "attributes"
    let t = fed(80, 24, b"\x1B[1mBold\x1B[0m");
    assert_eq!(text(&t), "Bold");
    assert!(style_at(&t, 0, 0).flags.bold);
    assert_eq!(t.screen().cursor.style, Style::default());
}

#[test]
fn sgr_style_refs_stay_exact() {
    // The cursor holds one reference to its style; each cell one more.
    let mut t = fed(10, 3, b"\x1b[1mAB\x1b[0m");
    let id = cell(&t, 0, 0).style_id();
    assert_ne!(id, 0);
    assert_eq!(t.screen().styles.refs(id), 2);
    t.feed(b"\x1b[H\x1b[2K");
    assert_eq!(t.screen().styles.refs(id), 0);
}

// ---- OSC: colours -------------------------------------------------------------------------

#[test]
fn osc4_query_reply() {
    let mut t = fed(10, 3, b"\x1b]4;1;?\x07");
    let c = crate::color::DEFAULT_PALETTE[1];
    let want = std::format!(
        "\x1b]4;1;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x07",
        c.r,
        c.r,
        c.g,
        c.g,
        c.b,
        c.b
    );
    assert_eq!(replies(&mut t), want.into_bytes());
}

#[test]
fn osc_4_set_and_reset_palette() {
    // stream_terminal.zig:1165 "OSC 4 set and reset palette"
    let mut t = term(10, 10);
    let default0 = t.colors().palette.original[0];
    t.feed(b"\x1b]4;0;rgb:ff/00/00\x1b\\");
    assert_eq!(t.colors().palette.current[0], rgb(0xff, 0, 0));
    assert!(t.colors().palette.mask.is_set(0));
    t.feed(b"\x1b]104;0\x1b\\");
    assert_eq!(t.colors().palette.current[0], default0);
    assert!(!t.colors().palette.mask.is_set(0));
}

#[test]
fn osc_104_reset_all_palette_colors() {
    // stream_terminal.zig:1188 "OSC 104 reset all palette colors"
    let mut t = fed(10, 10, b"\x1b]4;0;rgb:ff/00/00\x1b\\\x1b]4;1;rgb:00/ff/00\x1b\\");
    t.feed(b"\x1b]4;2;rgb:00/00/ff\x1b\\");
    for i in 0..3 {
        assert!(t.colors().palette.mask.is_set(i));
    }
    t.feed(b"\x1b]104\x1b\\");
    for i in 0..3u8 {
        let p = &t.colors().palette;
        assert_eq!(p.current[usize::from(i)], p.original[usize::from(i)]);
        assert!(!p.mask.is_set(i));
    }
}

#[test]
fn osc_10_11_12_set_and_reset() {
    // stream_terminal.zig:1213, 1235, 1254 "OSC 10/11/12 set and reset …"
    let mut t = term(10, 10);
    assert_eq!(t.colors().foreground.get(), None);
    t.feed(b"\x1b]10;rgb:ff/00/00\x1b\\");
    assert_eq!(t.colors().foreground.get(), Some(rgb(0xff, 0, 0)));
    t.feed(b"\x1b]110\x1b\\");
    assert_eq!(t.colors().foreground.get(), None);
    t.feed(b"\x1b]11;rgb:00/ff/00\x1b\\");
    assert_eq!(t.colors().background.get(), Some(rgb(0, 0xff, 0)));
    t.feed(b"\x1b]111\x1b\\");
    assert_eq!(t.colors().background.get(), None);
    t.feed(b"\x1b]12;rgb:00/00/ff\x1b\\");
    assert_eq!(t.colors().cursor.get(), Some(rgb(0, 0, 0xff)));
}

#[test]
fn osc_color_query_responses() {
    // stream_terminal.zig:1273 "OSC color query responses"
    let mut t = term(10, 10);
    t.feed(b"\x1b]10;?\x1b\\");
    assert_eq!(replies(&mut t), b"");
    t.feed(b"\x1b]11;?\x1b\\");
    assert_eq!(replies(&mut t), b"");
    t.feed(b"\x1b]4;2;rgb:12/34/56;2;?\x1b\\");
    assert_eq!(replies(&mut t), b"\x1b]4;2;rgb:1212/3434/5656\x1b\\");
    t.feed(b"\x1b]10;rgb:01/02/03\x1b\\\x1b]11;rgb:04/05/06\x1b\\\x1b]12;rgb:07/08/09\x1b\\");
    t.feed(b"\x1b]10;?;?;?\x1b\\");
    assert_eq!(
        replies(&mut t),
        b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]12;rgb:0707/0808/0909\x1b\\"
    );
    t.feed(b"\x1b]112\x1b\\\x1b]12;?\x07");
    assert_eq!(replies(&mut t), b"\x1b]12;rgb:0101/0202/0303\x07");
}

#[test]
fn kitty_color_protocol_set_and_reset() {
    // stream_terminal.zig:1330 "kitty color protocol set palette", 1408 "… reset foreground"
    let mut t = fed(10, 10, b"\x1b]21;5=rgb:ff/00/ff\x1b\\");
    assert_eq!(t.colors().palette.current[5], rgb(0xff, 0, 0xff));
    assert!(t.colors().palette.mask.is_set(5));
    assert_eq!(effects(&mut t).0, [Effect::PaletteChanged]);
    t.feed(b"\x1b]21;foreground=rgb:11/22/33\x1b\\");
    assert!(t.colors().foreground.get().is_some());
    t.feed(b"\x1b]21;foreground=\x1b\\");
    assert_eq!(t.colors().foreground.get(), None);
}

#[test]
fn kitty_color_protocol_query_responses() {
    // stream_terminal.zig:1424 "kitty color protocol query responses"
    let mut t = fed(10, 10, b"\x1b]21;background=?\x1b\\");
    assert_eq!(replies(&mut t), b"\x1b]21;background=\x1b\\");
    t.feed(b"\x1b]21;foreground=rgb:12/34/56;2=rgb:aa/bb/cc\x1b\\");
    t.feed(b"\x1b]21;foreground=?;background=?;2=?\x1b\\");
    assert_eq!(
        replies(&mut t),
        b"\x1b]21;foreground=rgb:12/34/56;background=;2=rgb:aa/bb/cc\x1b\\"
    );
}

#[test]
fn palette_dirty_flag_set_on_color_change() {
    // stream_terminal.zig:1464 "palette dirty flag set on color change"
    let mut t = term(10, 10);
    for seq in
        [&b"\x1b]4;0;rgb:ff/00/00\x1b\\"[..], b"\x1b]104;0\x1b\\", b"\x1b]21;1=rgb:00/ff/00\x1b\\"]
    {
        t.feed(seq);
        assert_eq!(effects(&mut t).0, [Effect::PaletteChanged], "{seq:?}");
    }
    // A dynamic colour is not the palette.
    t.feed(b"\x1b]10;rgb:01/02/03\x1b\\");
    assert_eq!(effects(&mut t).0, []);
}

#[test]
fn ignores_query_actions() {
    // stream_terminal.zig:1144 "ignores query actions": the terminal goes on.
    let mut t = fed(80, 24, b"\x1B[c\x1B[5n\x1B[6n\x1B]4;0;?\x1B\\\x1B]21;foreground=?\x1B\\");
    t.feed(b"Test");
    assert_eq!(text(&t), "Test");
}

// ---- OSC: titles, pwd, hyperlinks ---------------------------------------------------------

#[test]
fn window_title_effect_is_called() {
    // stream_terminal.zig:1865 "window_title effect is called"
    let mut t = fed(80, 24, b"\x1b]2;Hello World\x1b\\");
    assert_eq!(t.title(), b"Hello World");
    assert_eq!(effects(&mut t).0, [Effect::TitleChanged]);
}

#[test]
fn window_title_effect_with_empty_title() {
    // stream_terminal.zig:1909 "window_title effect with empty title"
    let mut t = fed(80, 24, b"\x1b]2;\x1b\\");
    assert_eq!(t.title(), b"");
    assert_eq!(effects(&mut t).0, [Effect::TitleChanged]);
}

#[test]
fn title_not_utf8_is_ignored_and_long_is_cut() {
    let mut t = fed(80, 24, b"\x1b]0;a\xffb\x07");
    assert_eq!(t.title(), b"");
    assert_eq!(effects(&mut t).0, []);
    let mut long = StdVec::from(&b"\x1b]2;"[..]);
    long.extend(core::iter::repeat_n(b'x', 1500));
    long.push(0x07);
    t.feed(&long);
    assert_eq!(t.title().len(), 1024);
}

#[test]
fn osc1_icon_is_ignored() {
    let mut t = fed(80, 24, b"\x1b]1;icon\x07");
    assert_eq!(t.title(), b"");
    assert_eq!(effects(&mut t).0, []);
    assert_eq!(t.stats().0.unsupported, 1);
}

#[test]
fn osc7_local_pwd() {
    let mut o = opts(20, 3);
    o.hostname = b"box";
    let mut t = Terminal::new_in(o, crate::test_support::heap()).expect("the test heap");
    t.feed(b"\x1b]7;file://box/home/a%20b?q#f\x07");
    assert_eq!(t.pwd(), b"/home/a b");
    assert_eq!(effects(&mut t).0, [Effect::PwdChanged]);
    t.feed(b"\x1b]7;kitty-shell-cwd://localhost/x%20y?z\x07");
    assert_eq!(t.pwd(), b"/x%20y?z");
    // Another host, no host, another scheme: ignored.
    for url in [&b"file://elsewhere/tmp"[..], b"file:///tmp", b"http://box/tmp"] {
        let mut seq = StdVec::from(&b"\x1b]7;"[..]);
        seq.extend_from_slice(url);
        seq.push(0x07);
        t.feed(&seq);
        assert_eq!(t.pwd(), b"/x%20y?z", "{url:?}");
    }
    effects(&mut t);
    // Empty clears.
    t.feed(b"\x1b]7;\x07");
    assert_eq!(t.pwd(), b"");
    assert_eq!(effects(&mut t).0, [Effect::PwdChanged]);
}

#[test]
fn osc8_hyperlink_on_named_cell() {
    let t = fed(10, 3, b"a\x1b]8;;http://a\x1b\\L\x1b]8;;\x1b\\b");
    let s = t.screen();
    assert_eq!(s.cell_link(cell(&t, 0, 0)), 0);
    let id = s.cell_link(cell(&t, 1, 0));
    assert_ne!(id, 0);
    assert_eq!(s.links.get(id).map(|(uri, _)| uri), Some(&b"http://a"[..]));
    assert_eq!(s.cell_link(cell(&t, 2, 0)), 0);
}

#[test]
fn osc22_mouse_shape() {
    let mut t = term(10, 3);
    assert_eq!(t.mouse_shape(), MouseShape::Text);
    t.feed(b"\x1b]22;pointer\x07\x1b]22;nonsense\x07");
    assert_eq!(t.mouse_shape(), MouseShape::Pointer);
    assert_eq!(effects(&mut t).0, [Effect::MouseShape(MouseShape::Pointer)]);
}

#[test]
fn osc9_notifications_and_progress() {
    let mut t = fed(10, 3, b"\x1b]9;hi there\x07\x1b]777;notify;T;B\x07\x1b]9;4;1;50\x07");
    let (list, fx) = effects(&mut t);
    let [
        Effect::DesktopNotification { title: t1, body: b1 },
        Effect::DesktopNotification { title: t2, body: b2 },
        Effect::Progress(p),
    ] = list[..]
    else {
        panic!("{list:?}");
    };
    assert_eq!((fx.bytes(t1), fx.bytes(b1)), (&b""[..], &b"hi there"[..]));
    assert_eq!((fx.bytes(t2), fx.bytes(b2)), (&b"T"[..], &b"B"[..]));
    assert_eq!(p, ProgressReport { state: ProgressState::Set, progress: Some(50) });
}

#[test]
fn osc_bounded_capture_is_invalid_past_max_buf() {
    // A title of more than 2048 bytes never parses (osc.zig MAX_BUF), and the capture stops.
    let mut seq = StdVec::from(&b"\x1b]2;"[..]);
    seq.extend(core::iter::repeat_n(b'x', crate::osc::MAX_BUF + 100));
    seq.push(0x07);
    let mut t = fed(10, 3, &seq);
    assert_eq!(t.title(), b"");
    assert_eq!(effects(&mut t).0, []);
    assert_eq!(t.stats().0.strings_truncated, 1);
}

// ---- OSC 52 -------------------------------------------------------------------------------

#[test]
fn clipboard_write_effect_callback() {
    // stream_terminal.zig:1652 "clipboard_write effect callback"
    let mut t = term(80, 24);
    let cases: [(&[u8], Clipboard, &[u8]); 6] = [
        (b"\x1B]52;c;aGVsbG8=\x1B\\", Clipboard::Standard, b"hello"),
        (b"\x1B]52;s;d29ybGQ=\x07", Clipboard::Selection, b"world"),
        (b"\x1B]52;p;cHJpbWFyeQ==\x1B\\", Clipboard::Primary, b"primary"),
        (b"\x1B]52;0;Y3V0\x1B\\", Clipboard::Standard, b"cut"),
        (b"\x1B]52;x;ZmFsbGJhY2s=\x1B\\", Clipboard::Standard, b"fallback"),
        (b"\x1B]52;c;YQBi\x1B\\", Clipboard::Standard, b"a\x00b"),
    ];
    for (seq, want_clip, want) in cases {
        t.feed(seq);
        let (list, fx) = effects(&mut t);
        let [Effect::ClipboardWrite { clipboard, contents }] = list[..] else { panic!("{list:?}") };
        assert_eq!(clipboard, want_clip);
        assert_eq!(fx.bytes(contents), want);
    }
    t.feed(b"\x1B]52;s;\x1B\\");
    let (list, _) = effects(&mut t);
    assert_eq!(
        list,
        [Effect::ClipboardWrite { clipboard: Clipboard::Selection, contents: Span::default() }]
    );
    // `?` reads; not-base64 writes nothing.
    t.feed(b"\x1B]52;c;?\x1B\\\x1B]52;c;***\x1B\\");
    assert_eq!(effects(&mut t).0, [Effect::ClipboardRead(Clipboard::Standard)]);
    t.feed(b"\x1B]1337;Copy=:aVRlcm0y\x1B\\");
    let (list, fx) = effects(&mut t);
    let [Effect::ClipboardWrite { clipboard: Clipboard::Standard, contents }] = list[..] else {
        panic!("{list:?}")
    };
    assert_eq!(fx.bytes(contents), b"iTerm2");
    t.feed(b"\x1B]52;p;ZnJh");
    t.feed(b"Z21lbnRlZA==\x1B");
    t.feed(b"\\");
    let (list, fx) = effects(&mut t);
    let [Effect::ClipboardWrite { clipboard: Clipboard::Primary, contents }] = list[..] else {
        panic!("{list:?}")
    };
    assert_eq!(fx.bytes(contents), b"fragmented");
}

#[test]
fn clipboard_read_reply() {
    let mut t = fed(10, 3, b"\x1b]52;s;?\x07");
    assert_eq!(effects(&mut t).0, [Effect::ClipboardRead(Clipboard::Selection)]);
    t.clipboard_reply(Clipboard::Selection, b"hello");
    assert_eq!(replies(&mut t), b"\x1b]52;s;aGVsbG8=\x1b\\");
}

// ---- OSC 133 ------------------------------------------------------------------------------

#[test]
fn semantic_prompt_fresh_line() {
    // stream_terminal.zig:1489 "semantic prompt fresh line"
    let t = fed(10, 10, b"Hello\x1b]133;L\x07");
    assert_eq!(cursor(&t), (0, 1));
}

#[test]
fn semantic_prompt_fresh_line_new_prompt() {
    // stream_terminal.zig:1502 "semantic prompt fresh line new prompt"
    let mut t = fed(10, 10, b"Hello\x1b]133;A\x07");
    assert_eq!(cursor(&t), (0, 1));
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Prompt);
    t.feed(b"prompt$ \x1b]133;A;redraw=0\x07");
    assert_eq!(t.flags().shell_redraws_prompt, PromptRedraw::False);
    t.feed(b"\x1b]133;A;redraw=1\x07");
    assert_eq!(t.flags().shell_redraws_prompt, PromptRedraw::True);
}

#[test]
fn semantic_prompt_end_of_input_then_start_output() {
    // stream_terminal.zig:1526 "semantic prompt end of input, then start output"
    let mut t = fed(10, 10, b"Hello\x1b]133;A\x07prompt$ \x1b]133;B\x07");
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Input);
    t.feed(b"\x1b]133;C\x07");
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Output);
    assert_eq!(effects(&mut t).0, [Effect::CommandStarted]);
}

#[test]
fn semantic_prompt_prompt_start() {
    // stream_terminal.zig:1543 "semantic prompt prompt_start"
    let t = fed(10, 10, b"Hello\x1b]133;P\x07");
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Prompt);
    assert_eq!(cursor(&t), (5, 0));
}

#[test]
fn semantic_prompt_new_command() {
    // stream_terminal.zig:1560 "semantic prompt new_command"
    let t = fed(10, 10, b"Hello\x1b]133;N\x07");
    assert_eq!(cursor(&t), (0, 1));
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Prompt);
}

#[test]
fn semantic_prompt_new_command_at_column_zero() {
    // stream_terminal.zig:1578 "semantic prompt new_command at column zero"
    let t = fed(10, 10, b"\x1b]133;N\x07");
    assert_eq!(cursor(&t), (0, 0));
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Prompt);
}

#[test]
fn semantic_prompt_end_prompt_start_input_terminate_eol_clears_on_linefeed() {
    // stream_terminal.zig:1592 "semantic prompt end_prompt_start_input_terminate_eol clears on
    // linefeed"
    let mut t = fed(10, 10, b"\x1b]133;I\x07");
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Input);
    t.feed(b"\n");
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Output);
}

#[test]
fn terminal_semantic_prompt() {
    // Terminal.zig:13238 "Terminal: semantic prompt"
    let t = fed(10, 5, b"\x1b]133;A\x07hello");
    assert_eq!(cursor(&t), (5, 0));
    assert_eq!(semantic_at(&t, 4, 0), SemanticContent::Prompt);
    assert_eq!(row_prompt(&t, 0), RowPrompt::Prompt);
    assert!(t.screen().semantic_prompt.seen);
    let t = fed(10, 5, b"\x1b]133;A\x07hello\x1b]133;I\x07\r\nworld");
    assert_eq!(cursor(&t), (5, 1));
    assert_eq!(semantic_at(&t, 4, 1), SemanticContent::Output);
    assert_eq!(row_prompt(&t, 1), RowPrompt::None);
}

#[test]
fn terminal_semantic_prompt_continuations() {
    // Terminal.zig:13282 "Terminal: semantic prompt continuations"
    let t = fed(10, 5, b"\x1b]133;A\x07hello\r\n\x1b]133;P;k=c\x07world");
    assert_eq!(cursor(&t), (5, 1));
    assert_eq!(semantic_at(&t, 4, 1), SemanticContent::Prompt);
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
}

#[test]
fn terminal_index_in_prompt_mode_marks_new_row_as_prompt_continuation() {
    // Terminal.zig:13329 "Terminal: index in prompt mode marks new row as prompt continuation"
    let t = fed(10, 5, b"\x1b]133;P\x07hello\r\n");
    assert_eq!(row_prompt(&t, 0), RowPrompt::Prompt);
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Prompt);
}

#[test]
fn terminal_index_in_input_mode_does_not_mark_new_row_as_prompt() {
    // Terminal.zig:13367 "Terminal: index in input mode does not mark new row as prompt" (the
    // row is a continuation all the same, as ghostty's test says)
    let t = fed(10, 5, b"\x1b]133;P\x07$ \x1b]133;B\x07echo \\\r\n");
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
    assert_eq!(t.screen().cursor.semantic, SemanticContent::Input);
}

#[test]
fn terminal_index_in_output_mode_does_not_mark_new_row_as_prompt() {
    // Terminal.zig:13397 "Terminal: index in output mode does not mark new row as prompt"
    let t = fed(10, 5, b"\x1b]133;P\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    assert_eq!(row_prompt(&t, 1), RowPrompt::None);
}

#[test]
fn terminal_osc133c_at_x0_on_prompt_row_clears_prompt_mark() {
    // Terminal.zig:13424 "Terminal: OSC133C at x=0 on prompt row clears prompt mark"
    let mut t = fed(10, 5, b"\x1b]133;P\x07$ echo \\\r\n");
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
    assert_eq!(cursor(&t).0, 0);
    t.feed(b"\x1b]133;C\x07");
    assert_eq!(row_prompt(&t, 1), RowPrompt::None);
}

#[test]
fn terminal_osc133c_at_x_gt_0_on_prompt_row_does_not_clear_prompt_mark() {
    // Terminal.zig:13463 "Terminal: OSC133C at x>0 on prompt row does not clear prompt mark"
    let mut t = fed(10, 5, b"\x1b]133;P\x07$ \r\n\x1b]133;P;k=c\x07> ");
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
    t.feed(b"\x1b]133;C\x07");
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
}

#[test]
fn terminal_multiple_newlines_in_prompt_mode_marks_all_rows() {
    // Terminal.zig:13505 "Terminal: multiple newlines in prompt mode marks all rows"
    let t = fed(10, 5, b"\x1b]133;P\x07line1\r\nline2\r\nline3");
    assert_eq!(row_prompt(&t, 0), RowPrompt::Prompt);
    assert_eq!(row_prompt(&t, 1), RowPrompt::Continuation);
    assert_eq!(row_prompt(&t, 2), RowPrompt::Continuation);
}

#[test]
fn osc133_command_effects_and_click() {
    let mut t = fed(10, 5, b"\x1b]133;A;click_events=1;cl=m\x07$ \x1b]133;C\x07");
    assert_eq!(
        t.screen().semantic_prompt.click,
        crate::screen::PromptClick::ClickEvents(crate::osc::ClickEvents::Absolute)
    );
    t.feed(b"\x1b]133;D;7\x07\x1b]133;D;300\x07\x1b]133;D\x07");
    assert_eq!(
        effects(&mut t).0,
        [
            Effect::CommandStarted,
            Effect::CommandFinished { exit_code: 7 },
            Effect::CommandFinished { exit_code: 1 },
            Effect::CommandFinished { exit_code: 0 },
        ]
    );
    t.feed(b"\x1b]133;A;cl=line\x07");
    assert_eq!(
        t.screen().semantic_prompt.click,
        crate::screen::PromptClick::Cl(crate::osc::Click::Line)
    );
}

// ---- DCS ----------------------------------------------------------------------------------

#[test]
fn decrqss_sgr_reply() {
    let mut t = fed(10, 3, b"\x1bP$qm\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP1$r0m\x1b\\");
    t.feed(b"\x1b[1;7m\x1bP$qm\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP1$r0;1;7m\x1b\\");
}

#[test]
fn decrqss_margins_and_cursor() {
    let mut t = fed(10, 6, b"\x1b[2;5r\x1bP$qr\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP1$r2;5r\x1b\\");
    t.feed(b"\x1bP$qs\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP0$r\x1b\\");
    t.feed(b"\x1b[?69h\x1b[3;7s\x1bP$qs\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP1$r3;7s\x1b\\");
    t.feed(b"\x1b[5 q\x1bP$q q\x1b\\");
    assert_eq!(replies(&mut t), b"\x1bP1$r5 q\x1b\\");
}

#[test]
fn xtgettcap_reply() {
    let mut t = fed(10, 3, b"\x1bP+q544e\x1b\\");
    let mut want = StdVec::from(&b"\x1bP1+r544E="[..]);
    for b in b"xterm-ghostty" {
        want.extend(std::format!("{b:02X}").into_bytes());
    }
    want.extend_from_slice(b"\x1b\\");
    assert_eq!(replies(&mut t), want);
}

#[test]
fn tmux_control_mode_effects() {
    let mut o = opts(20, 3);
    o.tmux_control_mode = true;
    let mut t = Terminal::new_in(o, crate::test_support::heap()).expect("the test heap");
    t.feed(b"\x1bP1000p%begin 1 2 3\n%end 1 2 3\n%output %1 hi\n\x1b\\");
    let (list, fx) = effects(&mut t);
    use crate::dcs::tmux::{Notification as N, PaneId};
    let [N::Enter, N::BlockEnd(block), N::Output { pane: PaneId(1), data }, N::Exit] = list
        .iter()
        .map(|e| match e {
            Effect::Tmux(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect::<StdVec<_>>()[..]
    else {
        panic!("{list:?}")
    };
    assert_eq!(fx.bytes(block), b"");
    assert_eq!(fx.bytes(data), b"hi");
    // Off by default: the hook is ignored.
    let mut t = fed(20, 3, b"\x1bP1000p%output %1 hi\n\x1b\\");
    assert_eq!(effects(&mut t).0, []);
}

// ---- RIS, glyphs --------------------------------------------------------------------------

#[test]
fn full_reset() {
    // stream_terminal.zig:1089 "full reset"
    let mut t = fed(80, 24, b"Hello\x1B[10;20H\x1B[5;20r\x1B[?7l");
    t.feed(b"\x1B_25a1;r;cp=e0a0;AAAAAAAAAAAAAA==\x1B\\");
    assert!(t.apc().glossary().contains(0xE0A0));
    t.feed(b"\x1Bc");
    assert_eq!(cursor(&t), (0, 0));
    assert_eq!((t.region().top, t.region().bottom), (0, 23));
    assert!(t.mode(crate::terminal::Mode::Wraparound));
    assert!(!t.apc().glossary().contains(0xE0A0));
}

#[test]
fn glyph_protocol_apc_with_write_pty_callback() {
    // stream_terminal.zig:1116 "glyph protocol APC with write_pty callback"
    let mut t = fed(80, 24, b"\x1B_25a1;s\x1B\\");
    assert_eq!(replies(&mut t), b"\x1B_25a1;s;fmt=glyf\x1B\\");
    t.feed(b"\x1B_25a1;r;cp=e0a0;AAAAAAAAAAAAAA==\x1B\\");
    assert_eq!(replies(&mut t), b"\x1B_25a1;r;cp=e0a0;status=0\x1B\\");
    assert!(t.apc().glossary().contains(0xE0A0));
}

// ---- kitty graphics -----------------------------------------------------------------------

/// A terminal whose cells are 10 × 20 pixels, so a 1 × 1 image takes one cell.
fn kitty_term(cols: u16, rows: u16) -> Terminal<'static> {
    let mut o = opts(cols, rows);
    o.cell_px = Some(Pixels { width: 10, height: 20 });
    Terminal::new_in(o, crate::test_support::heap()).expect("the test heap")
}

#[test]
fn kitty_graphics_apc_response() {
    // stream_terminal.zig:2581 "kitty graphics APC response", 2610 "kitty graphics via APC"
    let mut t = fed(10, 10, b"\x1b_Ga=t,t=d,f=24,i=1,s=1,v=2,c=10,r=1;////////\x1b\\");
    assert_eq!(replies(&mut t), b"\x1b_Gi=1;OK\x1b\\");
    let img = t.apc().graphics().storage(ScreenKey::Primary).image_by_id(1).expect("stored");
    assert_eq!(img.format, crate::kitty::command::Format::Rgb);
}

#[test]
fn kitty_direct_rgba_transmit_and_put() {
    let mut t = kitty_term(10, 5);
    t.feed(b"ab");
    // One RGBA pixel, transmitted and displayed at the cursor.
    t.feed(b"\x1b_Ga=T,f=32,s=1,v=1,i=7;AAAA/w==\x1b\\");
    assert_eq!(replies(&mut t), b"\x1b_Gi=7;OK\x1b\\");
    let st = t.apc().graphics().storage(ScreenKey::Primary);
    assert_eq!(st.image_count(), 1);
    assert_eq!(st.placement_count(), 1);
    let (_, p) = st.placements().next().expect("placed");
    let Location::Pin(pin) = p.location else { panic!("{p:?}") };
    let s = t.screen();
    assert_eq!(s.pin(pin.0), Some((s.r(0), 2)));
    // C=0: the cursor ends just right of the image, on its row.
    assert_eq!(cursor(&t), (3, 0));
}

#[test]
fn kitty_query_reply() {
    let mut t = kitty_term(10, 5);
    t.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert_eq!(replies(&mut t), b"\x1b_Gi=31;OK\x1b\\");
    // A query stores nothing.
    assert_eq!(t.apc().graphics().storage(ScreenKey::Primary).image_count(), 0);
}

#[test]
fn kitty_delete() {
    let mut t = kitty_term(10, 5);
    t.feed(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,q=2;AAAA/w==\x1b\\");
    t.feed(b"\x1b_Ga=T,f=32,s=1,v=1,i=2,q=2;AAAA/w==\x1b\\");
    let pins = t.screen().pins_tracked();
    assert_eq!(pins, 2);
    // `d=a`: every placement goes, the images stay; no reply.
    t.feed(b"\x1b_Ga=d\x1b\\");
    let st = t.apc().graphics().storage(ScreenKey::Primary);
    assert_eq!((st.image_count(), st.placement_count()), (2, 0));
    assert_eq!(t.screen().pins_tracked(), 0);
    assert_eq!(replies(&mut t), b"");
    // `d=I`: the image goes too.
    t.feed(b"\x1b_Ga=d,d=I,i=1\x1b\\");
    assert_eq!(t.apc().graphics().storage(ScreenKey::Primary).image_count(), 1);
}

#[test]
fn kitty_erase_display_and_reset() {
    let mut t = kitty_term(10, 5);
    t.feed(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,q=2;AAAA/w==\x1b\\");
    t.feed(b"\x1b[2J");
    let st = t.apc().graphics().storage(ScreenKey::Primary);
    assert_eq!((st.image_count(), st.placement_count()), (1, 0));
    t.feed(b"\x1b_Ga=p,i=1,q=2\x1b\\\x1bc");
    let st = t.apc().graphics().storage(ScreenKey::Primary);
    assert_eq!((st.image_count(), st.placement_count()), (0, 0));
    assert_eq!(t.screen().pins_tracked(), 0);
}

#[test]
fn kitty_pins_follow_scrolling() {
    // The alternate screen keeps no scrollback: a scroll moves the rows, and the pin with them.
    let mut t = kitty_term(10, 5);
    t.feed(b"\x1b[?1049h\x1b[3;1H\x1b_Ga=T,f=32,s=1,v=1,i=1,q=2,C=1;AAAA/w==\x1b\\");
    let pin_row = |t: &Terminal<'_>| {
        let st = t.apc().graphics().storage(ScreenKey::Alternate);
        let (_, p) = st.placements().next().expect("placed");
        let Location::Pin(pin) = p.location else { panic!("{p:?}") };
        let s = t.screen();
        s.pin(pin.0).map(|(r, _)| r - s.pages.active_top())
    };
    assert_eq!(pin_row(&t), Some(2));
    t.feed(b"\x1b[5;1H\n\n");
    assert_eq!(pin_row(&t), Some(0));
    // On the primary screen the row goes to scrollback: the pin stays on its row.
    let mut t = kitty_term(10, 3);
    t.feed(b"\x1b[3;1H\x1b_Ga=T,f=32,s=1,v=1,i=1,q=2,C=1;AAAA/w==\x1b\\\n\n");
    let st = t.apc().graphics().storage(ScreenKey::Primary);
    let (_, p) = st.placements().next().expect("placed");
    let Location::Pin(pin) = p.location else { panic!("{p:?}") };
    assert_eq!(t.screen().pin(pin.0), Some((2, 0)));
}

#[test]
fn kitty_file_load_waits_on_host() {
    let mut t = kitty_term(10, 5);
    t.apc_mut().graphics_mut().set_media(crate::kitty::image::Media::ALL);
    // t=f, the path base64: "/tmp/x.rgba".
    t.feed(b"\x1b_Ga=t,t=f,f=32,s=1,v=1,i=5;L3RtcC94LnJnYmE=\x1b\\");
    assert_eq!(effects(&mut t).0, [Effect::KittyIo]);
    assert!(matches!(
        t.kitty_pending(),
        Some(crate::kitty::exec::Request::Resolve { path: b"/tmp/x.rgba" })
    ));
    assert!(
        t.kitty_answer(crate::kitty::exec::Answer::Failed(crate::kitty::image::IoFailed)).is_ok()
    );
    let r = replies(&mut t);
    assert!(r.starts_with(b"\x1b_Gi=5;EINVAL"), "{:?}", std::string::String::from_utf8_lossy(&r));
    assert!(t.kitty_pending().is_none());
}

#[test]
fn kitty_placeholder_cells() {
    use crate::kitty::unicode::PlaceholderCells;
    // U+10EEEE with a row and column diacritic, in fg palette 5 (image id 5).
    let t = fed(10, 3, "\x1b[38;5;5m\u{10EEEE}\u{305}\u{305}x".as_bytes());
    let cells = t.kitty_cells();
    let s = t.screen();
    let r = s.r(0) as u64;
    assert!(cells.row_flagged(r));
    assert!(!cells.row_flagged(s.r(1) as u64));
    let c = cells.cell(crate::kitty::storage::ScreenPoint { row: r, col: 0 });
    assert_eq!(c.codepoint, '\u{10EEEE}');
    assert_eq!(c.fg, Color::Palette(5));
    assert_eq!(c.marks, &[0x305, 0x305]);
}
