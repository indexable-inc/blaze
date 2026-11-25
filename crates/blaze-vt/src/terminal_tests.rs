//! Terminal tests: ghostty's Terminal.zig tests ported (each names its line), the quirks kept
//! (each names ghostty's line), the HPA/VPA fix, and the replies.

use std::vec::Vec as StdVec;

use crate::page::Wide;
use crate::screen::ProtectedMode;
use crate::terminal::{Effect, Mode, Which};
use crate::test_support::{
    Case, all_text, cell, cursor, fed, history_text, replies, run_cases, term, text, wide_at,
};

#[test]
fn print_wrap_scroll_cases() {
    run_cases(&[
        Case {
            name: "input with no control characters",
            at: "Terminal.zig:3944",
            cols: 40,
            rows: 40,
            input: b"hello",
            text: "hello",
            cursor: (5, 0),
        },
        Case {
            name: "input with basic wraparound",
            at: "Terminal.zig:3964",
            cols: 5,
            rows: 40,
            input: b"helloworldabc12",
            text: "hello\nworld\nabc12",
            cursor: (4, 2),
        },
        Case {
            name: "input that forces scroll",
            at: "Terminal.zig:3996",
            cols: 1,
            rows: 5,
            input: b"abcdef",
            text: "b\nc\nd\ne\nf",
            cursor: (0, 4),
        },
        Case {
            name: "zero-width character at start",
            at: "Terminal.zig:4054",
            cols: 80,
            rows: 80,
            input: "\u{200D}".as_bytes(),
            text: "",
            cursor: (0, 0),
        },
        Case {
            name: "zero-width character attaches to pending wrap cell",
            at: "Terminal.zig:4070",
            cols: 2,
            rows: 2,
            input: "x\u{e5}\u{332}".as_bytes(),
            text: "x\u{e5}\u{332}",
            cursor: (1, 0),
        },
        Case {
            name: "print wide char",
            at: "Terminal.zig:4096",
            cols: 80,
            rows: 80,
            input: "\u{1F600}".as_bytes(),
            text: "\u{1F600}",
            cursor: (2, 0),
        },
        Case {
            name: "print wide char at edge creates spacer head",
            at: "Terminal.zig:4119",
            cols: 10,
            rows: 10,
            input: "\x1b[1;10H\u{1F600}".as_bytes(),
            text: "\n\u{1F600}",
            cursor: (2, 1),
        },
        Case {
            name: "print wide char in single-width terminal",
            at: "Terminal.zig:4164",
            cols: 1,
            rows: 80,
            input: "\u{1F600}".as_bytes(),
            text: "",
            cursor: (0, 0),
        },
        Case {
            name: "print over wide char at 0,0",
            at: "Terminal.zig:4183",
            cols: 80,
            rows: 80,
            input: "\u{1F600}\x1b[HA".as_bytes(),
            text: "A",
            cursor: (1, 0),
        },
        Case {
            name: "print over wide spacer tail",
            at: "Terminal.zig:4249",
            cols: 5,
            rows: 5,
            input: "\u{6a4b}\x1b[1;2HX".as_bytes(),
            text: " X",
            cursor: (2, 0),
        },
        Case {
            name: "soft wrap",
            at: "Terminal.zig:5757",
            cols: 3,
            rows: 80,
            input: b"hello",
            text: "hel\nlo",
            cursor: (2, 1),
        },
        Case {
            name: "disabled wraparound with wide char and one space",
            at: "Terminal.zig:5793",
            cols: 5,
            rows: 5,
            input: "\x1b[?7lAAAA\u{1F6A8}".as_bytes(),
            text: "AAAA",
            cursor: (4, 0),
        },
        Case {
            name: "disabled wraparound with wide char and no space",
            at: "Terminal.zig:5825",
            cols: 5,
            rows: 5,
            input: "\x1b[?7lAAAAA\u{1F6A8}".as_bytes(),
            text: "AAAAA",
            cursor: (4, 0),
        },
        Case {
            name: "print right margin wrap",
            at: "Terminal.zig:5889",
            cols: 10,
            rows: 5,
            input: b"123456789\x1b[?69h\x1b[3;5s\x1b[1;5HXY",
            text: "1234X6789\n  Y",
            cursor: (3, 1),
        },
        Case {
            name: "print right margin outside",
            at: "Terminal.zig:5941",
            cols: 10,
            rows: 5,
            input: b"123456789\x1b[?69h\x1b[3;5s\x1b[1;6HXY",
            text: "12345XY89",
            cursor: (7, 0),
        },
        Case {
            name: "print right margin outside wrap",
            at: "Terminal.zig:5961",
            cols: 10,
            rows: 5,
            input: b"123456789\x1b[?69h\x1b[3;5s\x1b[1;10HXY",
            text: "123456789X\n  Y",
            cursor: (3, 1),
        },
        Case {
            name: "linefeed and carriage return",
            at: "Terminal.zig:6212",
            cols: 80,
            rows: 80,
            input: b"hello\r\nworld",
            text: "hello\nworld",
            cursor: (5, 1),
        },
        Case {
            name: "linefeed unsets pending wrap",
            at: "Terminal.zig:6240",
            cols: 5,
            rows: 80,
            input: b"hello\n",
            text: "hello",
            cursor: (4, 1),
        },
        Case {
            name: "linefeed mode automatic carriage return",
            at: "Terminal.zig:6254",
            cols: 10,
            rows: 10,
            input: b"\x1b[20h123456\nX",
            text: "123456\nX",
            cursor: (1, 1),
        },
        Case {
            name: "carriage return unsets pending wrap",
            at: "Terminal.zig:6270",
            cols: 5,
            rows: 80,
            input: b"hello\r",
            text: "hello",
            cursor: (0, 0),
        },
        Case {
            name: "carriage return origin mode moves to left margin",
            at: "Terminal.zig:6281",
            cols: 5,
            rows: 80,
            input: b"\x1b[?6h\x1b[?69h\x1b[3;5s\x1b[1;1H\x1b[?6l\x1b[1;1H\x1b[?6h\r",
            text: "",
            cursor: (2, 0),
        },
        Case {
            name: "carriage return left of left margin moves to zero",
            at: "Terminal.zig:6292",
            cols: 5,
            rows: 80,
            input: b"\x1b[?69h\x1b[3;5s\x1b[1;2H\r",
            text: "",
            cursor: (0, 0),
        },
        Case {
            name: "carriage return right of left margin moves to left margin",
            at: "Terminal.zig:6302",
            cols: 5,
            rows: 80,
            input: b"\x1b[?69h\x1b[3;5s\x1b[1;4H\r",
            text: "",
            cursor: (2, 0),
        },
        Case {
            name: "backspace",
            at: "Terminal.zig:6312",
            cols: 80,
            rows: 80,
            input: b"hello\x08y",
            text: "helly",
            cursor: (5, 0),
        },
        Case {
            name: "horizontal tabs",
            at: "Terminal.zig:6329",
            cols: 20,
            rows: 5,
            input: b"1\t",
            text: "1",
            cursor: (8, 0),
        },
        Case {
            name: "horizontal tabs to the last column",
            at: "Terminal.zig:6329",
            cols: 20,
            rows: 5,
            input: b"1\t\t\t",
            text: "1",
            cursor: (19, 0),
        },
        Case {
            name: "horizontal tabs starting on tabstop",
            at: "Terminal.zig:6350",
            cols: 20,
            rows: 5,
            input: b"\x1b[1;9HX\x1b[1;9H\tA",
            text: "        X       A",
            cursor: (17, 0),
        },
        Case {
            name: "horizontal tabs with right margin",
            at: "Terminal.zig:6368",
            cols: 20,
            rows: 5,
            input: b"\x1b[?69h\x1b[3;6s\x1b[1;1HX\tA",
            text: "X    A",
            cursor: (5, 0),
        },
        Case {
            name: "horizontal tabs back",
            at: "Terminal.zig:6387",
            cols: 20,
            rows: 5,
            input: b"\x1b[1;20H\x1b[Z",
            text: "",
            cursor: (16, 0),
        },
        Case {
            name: "horizontal tabs back starting on tabstop",
            at: "Terminal.zig:6410",
            cols: 20,
            rows: 5,
            input: b"\x1b[1;9HX\x1b[1;9H\x1b[ZA",
            text: "A       X",
            cursor: (1, 0),
        },
        Case {
            name: "horizontal tabs with left margin in origin mode",
            at: "Terminal.zig:6428",
            cols: 20,
            rows: 5,
            input: b"\x1b[?6h\x1b[?69h\x1b[3;5s\x1b[1;2HX\x1b[ZA",
            text: "  AX",
            cursor: (3, 0),
        },
        Case {
            name: "horizontal tab back with cursor before left margin",
            at: "Terminal.zig:6448",
            cols: 20,
            rows: 5,
            input: b"\x1b[?6h\x1b7\x1b[?69h\x1b[5s\x1b8\x1b[ZA",
            text: "A",
            cursor: (1, 0),
        },
        Case {
            name: "cursorPos resets wrap",
            at: "Terminal.zig:6468",
            cols: 5,
            rows: 5,
            input: b"ABCDE\x1b[HX",
            text: "XBCDE",
            cursor: (1, 0),
        },
        Case {
            name: "cursorPos off the screen",
            at: "Terminal.zig:6486",
            cols: 5,
            rows: 5,
            input: b"\x1b[500;500HX",
            text: "\n\n\n\n    X",
            cursor: (4, 4),
        },
        Case {
            name: "cursorPos relative to origin",
            at: "Terminal.zig:6501",
            cols: 5,
            rows: 5,
            input: b"\x1b[3;4r\x1b[?6h\x1b[HX",
            text: "\n\nX",
            cursor: (1, 2),
        },
        Case {
            name: "cursorPos relative to origin with left/right",
            at: "Terminal.zig:6519",
            cols: 5,
            rows: 5,
            input: b"\x1b[3;4r\x1b[?69h\x1b[3;5s\x1b[?6h\x1b[HX",
            text: "\n\n  X",
            cursor: (3, 2),
        },
        Case {
            name: "cursorPos limits with full scroll region",
            at: "Terminal.zig:6539",
            cols: 5,
            rows: 5,
            input: b"\x1b[3;4r\x1b[?69h\x1b[3;5s\x1b[?6h\x1b[500;500HX",
            text: "\n\n\n    X",
            cursor: (4, 3),
        },
    ]);
}

#[test]
fn print_wide_char_at_edge_cells() {
    // Terminal.zig:4119
    let t = fed(10, 10, "\x1b[1;10H\u{1F600}".as_bytes());
    assert_eq!(wide_at(&t, 9, 0), Wide::SpacerHead);
    assert!(t.screen().pages.row(t.screen().r(0)).wrap);
    assert_eq!(cell(&t, 0, 1).codepoint(), 0x1F600);
    assert_eq!(wide_at(&t, 0, 1), Wide::Wide);
    assert_eq!(wide_at(&t, 1, 1), Wide::SpacerTail);
}

#[test]
fn print_over_wide_char_at_col_0_keeps_previous_row() {
    // Terminal.zig:4211: the previous row's last cell is a spacer tail, not a head; it stays.
    let mut t = term(10, 3);
    for _ in 0..10 {
        t.feed("\u{4E2D}".as_bytes());
    }
    t.feed(b"\x1b[2;1HA");
    assert_eq!(wide_at(&t, 0, 1), Wide::Narrow);
    assert_eq!(wide_at(&t, 8, 0), Wide::Wide);
    assert_eq!(wide_at(&t, 9, 0), Wide::SpacerTail);
}

#[test]
fn print_over_wide_char_with_bold_releases_the_style() {
    // Terminal.zig:4279
    let mut t = fed(80, 80, "\x1b[1m\u{1F600}".as_bytes());
    assert_eq!(t.screen().styles.live(), 1);
    t.feed(b"\x1b[H\x1b[0mA");
    assert_eq!(t.screen().styles.live(), 0);
}

#[test]
fn print_over_wide_char_with_bg_color() {
    // Terminal.zig:4305: the spacer tail cleared under a styled cursor gets its background.
    let mut t = fed(80, 80, "\x1b[41m\u{1F600}".as_bytes());
    t.feed(b"\x1b[H\x1b[0mA");
    assert_eq!(text(&t), "A");
    assert_eq!(wide_at(&t, 1, 0), Wide::Narrow);
    assert_eq!(t.screen().styles.live(), 0);
}

#[test]
fn print_charsets() {
    // Terminal.zig:5641 "print charset", 5670 "outside of ASCII", 5695 "invoke", 5719 "single".
    assert_eq!(text(&fed(80, 80, b"\x1b(0`")), "\u{25C6}");
    assert_eq!(text(&fed(80, 80, b"\x1b(B`")), "`");
    assert_eq!(text(&fed(80, 80, b"\x1b(A#")), "\u{a3}");
    assert_eq!(text(&fed(80, 80, "\x1b(0\u{1F600}".as_bytes())), " ");
    assert_eq!(text(&fed(80, 80, b"\x1b)0`\x0e`\x0f`")), "`\u{25C6}`");
}

#[test]
fn single_shift_is_used_once() {
    // stream.zig:2548 ESC N: one character from G2, then GL again.
    assert_eq!(text(&fed(80, 80, b"\x1b*0\x1bN``")), "\u{25C6}`");
    assert_eq!(text(&fed(80, 80, b"\x1b+0\x1bO`a`")), "\u{25C6}a`");
    // LS2 / LS3 lock.
    assert_eq!(text(&fed(80, 80, b"\x1b*0\x1bn``\x0f`")), "\u{25C6}\u{25C6}`");
    assert_eq!(text(&fed(80, 80, b"\x1b+0\x1bo``")), "\u{25C6}\u{25C6}");
}

#[test]
fn gr_shifts_are_stored_not_used() {
    // inventory §3: ESC ~ } | set GR, which printing never reads.
    let t = fed(80, 80, b"\x1b)0\x1b~`");
    assert_eq!(t.screen().charset.gr, crate::screen::Slot::G1);
    assert_eq!(text(&t), "`");
}

#[test]
fn soft_wrap_marks_rows() {
    // Terminal.zig:5757
    let t = fed(3, 80, b"hello");
    let s = t.screen();
    assert!(s.pages.row(s.r(0)).wrap);
    assert!(s.pages.row(s.r(1)).wrap_continuation);
}

#[test]
fn hpa_vpa_in_origin_mode_add_the_margin_once() {
    // A bug in ghostty fixed here: stream_terminal.zig:174-183 passes the absolute cursor to
    // setCursorPos, which adds the origin offset again. With top/left margins 2 and origin on,
    // ghostty's VPA 1 leaves the column at 2 + 2; blaze keeps it at 2.
    let mut t = term(10, 10);
    t.feed(b"\x1b[?69h\x1b[3;8s\x1b[3;8r\x1b[?6h");
    assert_eq!(cursor(&t), (2, 2));
    t.feed(b"\x1b[2d");
    assert_eq!(cursor(&t), (2, 3), "VPA keeps the column");
    t.feed(b"\x1b[3G");
    assert_eq!(cursor(&t), (4, 3), "CHA keeps the row");
    t.feed(b"\x1b[a");
    assert_eq!(cursor(&t), (5, 3), "HPR moves one column");
    t.feed(b"\x1b[e");
    assert_eq!(cursor(&t), (5, 4), "VPR moves one row");
    t.feed(b"\x1b[6n");
    assert_eq!(replies(&mut t), b"\x1b[3;4R", "CPR is origin-relative");
}

#[test]
fn reverse_wrap_cases() {
    // Terminal.zig cursorLeft (1551): xterm's reverse wrap.
    run_cases(&[
        Case {
            name: "cursorLeft no wrap",
            at: "Terminal.zig:1551",
            cols: 5,
            rows: 5,
            input: b"\x1b[2;1H\x1b[D",
            text: "",
            cursor: (0, 1),
        },
        Case {
            name: "cursorLeft reverse wrap onto a wrapped row",
            at: "Terminal.zig:1551",
            cols: 5,
            rows: 5,
            input: b"\x1b[?45hABCDEF\x1b[2DX",
            text: "ABCDX\nF",
            cursor: (4, 0),
        },
        Case {
            name: "cursorLeft reverse wrap stops at an unwrapped row",
            at: "Terminal.zig:1551",
            cols: 5,
            rows: 5,
            input: b"\x1b[?45hABC\r\nDE\x1b[5DX",
            text: "ABC\nXE",
            cursor: (1, 1),
        },
        Case {
            name: "cursorLeft extended reverse wrap ignores the wrap flag",
            at: "Terminal.zig:1551",
            cols: 5,
            rows: 5,
            input: b"\x1b[?1045hABC\r\nDE\x1b[3DX",
            text: "ABC X\nDE",
            cursor: (5 - 1, 0),
        },
    ]);
}

#[test]
fn scroll_cases() {
    run_cases(&[
        Case {
            name: "index at the bottom scrolls",
            at: "Terminal.zig:1969",
            cols: 5,
            rows: 3,
            input: b"A\nB\nC\nD",
            text: " B\n  C\n   D",
            cursor: (4, 2),
        },
        Case {
            name: "index outside the region just moves down",
            at: "Terminal.zig:1969",
            cols: 5,
            rows: 5,
            input: b"\x1b[1;2r\x1b[4;1HA\x1bD\x1bDB",
            text: "\n\n\nA\n B",
            cursor: (2, 4),
        },
        Case {
            name: "index at the region bottom scrolls the region",
            at: "Terminal.zig:1969",
            cols: 5,
            rows: 5,
            input: b"\x1b[2;3r\x1b[1;1H1\r\n2\r\n3\x1bD4\x1b[5;1H5",
            text: "1\n3\n 4\n\n5",
            cursor: (1, 4),
        },
        Case {
            name: "reverse index at the top scrolls down",
            at: "Terminal.zig:2082",
            cols: 5,
            rows: 3,
            input: b"A\r\nB\r\nC\x1b[H\x1bMX",
            text: "X\nA\nB",
            cursor: (1, 0),
        },
        Case {
            name: "scrollUp simple",
            at: "Terminal.zig:7329",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[S",
            text: "DEF\nGHI",
            cursor: (1, 1),
        },
        Case {
            name: "scrollUp top/bottom scroll region",
            at: "Terminal.zig:7450",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;3r\x1b[1;1H\x1b[S",
            text: "ABC\nGHI",
            cursor: (0, 0),
        },
        Case {
            name: "scrollUp left/right scroll region",
            at: "Terminal.zig:7480",
            cols: 10,
            rows: 10,
            input: b"ABC123\r\nDEF456\r\nGHI789\x1b[?69h\x1b[2;4s\x1b[2;2H\x1b[S",
            text: "AEF423\nDHI756\nG   89",
            cursor: (1, 1),
        },
        Case {
            name: "scrollDown simple",
            at: "Terminal.zig:2185",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[T",
            text: "\nABC\nDEF\nGHI",
            cursor: (1, 1),
        },
        Case {
            name: "insertLines simple",
            at: "Terminal.zig:6883",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[L",
            text: "ABC\n\nDEF\nGHI",
            cursor: (0, 1),
        },
        Case {
            name: "insertLines outside of scroll region",
            at: "Terminal.zig:6990",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[3;4r\x1b[2;2H\x1b[L",
            text: "ABC\nDEF\nGHI",
            cursor: (1, 1),
        },
        Case {
            name: "insertLines top/bottom scroll region",
            at: "Terminal.zig:7019",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\r\n123\x1b[1;3r\x1b[2;2H\x1b[L",
            text: "ABC\n\nDEF\n123",
            cursor: (0, 1),
        },
        Case {
            name: "insertLines more than remaining",
            at: "Terminal.zig:7184",
            cols: 2,
            rows: 5,
            input: b"A\r\nB\r\nC\r\nD\r\nE\x1b[2;1H\x1b[20L",
            text: "A",
            cursor: (0, 1),
        },
        Case {
            name: "insertLines left/right scroll region",
            at: "Terminal.zig:7298",
            cols: 10,
            rows: 10,
            input: b"ABC123\r\nDEF456\r\nGHI789\x1b[?69h\x1b[2;4s\x1b[2;2H\x1b[L",
            text: "ABC123\nD   56\nGEF489\n HI7",
            cursor: (1, 1),
        },
        Case {
            name: "deleteLines simple",
            at: "Terminal.zig:2677",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[M",
            text: "ABC\nGHI",
            cursor: (0, 1),
        },
        Case {
            name: "deleteLines left/right scroll region",
            at: "Terminal.zig:2677",
            cols: 10,
            rows: 10,
            input: b"ABC123\r\nDEF456\r\nGHI789\x1b[?69h\x1b[2;4s\x1b[2;2H\x1b[M",
            text: "ABC123\nDHI756\nG   89",
            cursor: (1, 1),
        },
    ]);
}

#[test]
fn scroll_up_keeps_pending_wrap() {
    // Terminal.zig:2207: SU keeps the cursor and its pending wrap.
    let t = fed(5, 5, b"ABCDE\x1b[S");
    assert!(t.screen().cursor.pending_wrap);
    assert_eq!(cursor(&t), (4, 0));
}

#[test]
fn insert_lines_resets_pending_wrap() {
    // Terminal.zig:7222
    let t = fed(5, 5, b"ABCDE\x1b[L");
    assert!(!t.screen().cursor.pending_wrap);
    assert_eq!(cursor(&t), (0, 0));
    // Terminal.zig:7135 "insertLines zero": CSI 0 L does nothing.
    let t = fed(5, 5, b"ABC\x1b[0L");
    assert_eq!((text(&t), cursor(&t)), ("ABC".into(), (3, 0)));
}

#[test]
fn scrollback_from_index_and_su() {
    let t = fed(5, 3, b"1\r\n2\r\n3\r\n4\r\n5");
    assert_eq!(history_text(&t), "1\n2");
    assert_eq!(text(&t), "3\n4\n5");
    // SU with a full-screen region pushes into scrollback; a partial region does not.
    let t = fed(5, 3, b"1\r\n2\r\n3\x1b[2S");
    assert_eq!(history_text(&t), "1\n2");
    let t = fed(5, 3, b"1\r\n2\r\n3\x1b[2;3r\x1b[S");
    assert_eq!(history_text(&t), "");
    assert_eq!(text(&t), "1\n3");
}

#[test]
fn erase_cases() {
    run_cases(&[
        Case {
            name: "eraseLine right",
            at: "Terminal.zig:3072",
            cols: 10,
            rows: 5,
            input: b"ABCDE\x1b[1;3H\x1b[K",
            text: "AB",
            cursor: (2, 0),
        },
        Case {
            name: "eraseLine left",
            at: "Terminal.zig:3087",
            cols: 10,
            rows: 5,
            input: b"ABCDE\x1b[1;3H\x1b[1K",
            text: "   DE",
            cursor: (2, 0),
        },
        Case {
            name: "eraseLine complete",
            at: "Terminal.zig:3100",
            cols: 10,
            rows: 5,
            input: b"ABCDE\x1b[1;3H\x1b[2K",
            text: "",
            cursor: (2, 0),
        },
        Case {
            name: "eraseLine right of a spacer tail takes the wide char",
            at: "Terminal.zig:3072",
            cols: 10,
            rows: 5,
            input: "A\u{6a4b}B\x1b[1;3H\x1b[K".as_bytes(),
            text: "A",
            cursor: (2, 0),
        },
        Case {
            name: "eraseLine left on a wide char takes its tail",
            at: "Terminal.zig:3087",
            cols: 10,
            rows: 5,
            input: "A\u{6a4b}B\x1b[1;2H\x1b[1K".as_bytes(),
            text: "   B",
            cursor: (1, 0),
        },
        Case {
            name: "eraseDisplay below",
            at: "Terminal.zig:3236",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[J",
            text: "ABC\nD",
            cursor: (1, 1),
        },
        Case {
            name: "eraseDisplay above",
            at: "Terminal.zig:3253",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[1J",
            text: "\n  F\nGHI",
            cursor: (1, 1),
        },
        Case {
            name: "eraseDisplay complete",
            at: "Terminal.zig:3176",
            cols: 5,
            rows: 5,
            input: b"ABC\r\nDEF\r\nGHI\x1b[2;2H\x1b[2J",
            text: "",
            cursor: (1, 1),
        },
        Case {
            name: "eraseChars simple",
            at: "Terminal.zig:3013",
            cols: 5,
            rows: 5,
            input: b"ABC\x1b[1;1H\x1b[2X",
            text: "  C",
            cursor: (0, 0),
        },
        Case {
            name: "eraseChars zero is one",
            at: "Terminal.zig:3013",
            cols: 5,
            rows: 5,
            input: b"ABC\x1b[1;1H\x1b[0X",
            text: " BC",
            cursor: (0, 0),
        },
        Case {
            name: "eraseChars beyond the end",
            at: "Terminal.zig:3013",
            cols: 5,
            rows: 5,
            input: b"ABC\x1b[1;2H\x1b[10X",
            text: "A",
            cursor: (1, 0),
        },
        Case {
            name: "eraseChars extends over a wide char",
            at: "Terminal.zig:3013",
            cols: 5,
            rows: 5,
            input: "A\u{6a4b}B\x1b[1;1H\x1b[2X".as_bytes(),
            text: "   B",
            cursor: (0, 0),
        },
        Case {
            name: "insertBlanks",
            at: "Terminal.zig:2862",
            cols: 5,
            rows: 2,
            input: b"ABC\x1b[1;1H\x1b[2@",
            text: "  ABC",
            cursor: (0, 0),
        },
        Case {
            name: "insertBlanks pushes off the end",
            at: "Terminal.zig:2862",
            cols: 5,
            rows: 2,
            input: b"ABCDE\x1b[1;1H\x1b[2@",
            text: "  ABC",
            cursor: (0, 0),
        },
        Case {
            name: "insertBlanks zero is one",
            at: "stream.zig:2214",
            cols: 5,
            rows: 2,
            input: b"ABC\x1b[1;1H\x1b[0@",
            text: " ABC",
            cursor: (0, 0),
        },
        Case {
            name: "deleteChars",
            at: "Terminal.zig:2963",
            cols: 5,
            rows: 2,
            input: b"ABCDE\x1b[1;2H\x1b[2P",
            text: "ADE",
            cursor: (1, 0),
        },
        Case {
            name: "deleteChars zero is nothing",
            at: "Terminal.zig:2963",
            cols: 5,
            rows: 2,
            input: b"ABCDE\x1b[1;2H\x1b[0P",
            text: "ABCDE",
            cursor: (1, 0),
        },
        Case {
            name: "deleteChars with a right margin",
            at: "Terminal.zig:2963",
            cols: 10,
            rows: 2,
            input: b"ABC123\x1b[?69h\x1b[1;4s\x1b[1;2H\x1b[P",
            text: "AC1 23",
            cursor: (1, 0),
        },
        Case {
            name: "insert mode",
            at: "Terminal.zig:882",
            cols: 10,
            rows: 2,
            input: b"hello\x1b[1;2H\x1b[4hXY",
            text: "hXYello",
            cursor: (3, 0),
        },
        Case {
            name: "DECALN",
            at: "Terminal.zig:3277",
            cols: 2,
            rows: 2,
            input: b"A\r\nB\x1b#8",
            text: "EE\nEE",
            cursor: (0, 0),
        },
        Case {
            name: "REP",
            at: "stream.zig:1572",
            cols: 10,
            rows: 2,
            input: b"A\x1b[3b",
            text: "AAAA",
            cursor: (4, 0),
        },
        Case {
            name: "REP with nothing printed",
            at: "stream.zig:1572",
            cols: 10,
            rows: 2,
            input: b"\x1b[3b",
            text: "",
            cursor: (0, 0),
        },
    ]);
}

#[test]
fn erase_line_resets_wrap_but_complete_does_not() {
    // Terminal.zig:3072 (right resets soft wrap), 3100 (complete keeps it, as xterm).
    let t = fed(3, 5, b"ABCDE\x1b[1;2H\x1b[K");
    assert!(!t.screen().pages.row(t.screen().r(0)).wrap);
    let t = fed(3, 5, b"ABCDE\x1b[1;2H\x1b[2K");
    assert!(t.screen().pages.row(t.screen().r(0)).wrap);
}

#[test]
fn protection() {
    // Terminal.zig:1709. DECSCA protects from DECSED/DECSEL only; SPA from every erase, and
    // the ISO mode stays after EPA (a quirk kept: inventory §3 ESC W).
    let t = fed(10, 2, b"\x1b[1\"qAB\x1b[0\"qC\x1b[1;1H\x1b[?K");
    assert_eq!(text(&t), "AB");
    let t = fed(10, 2, b"\x1b[1\"qAB\x1b[0\"qC\x1b[1;1H\x1b[K");
    assert_eq!(text(&t), "");
    let t = fed(10, 2, b"\x1bVAB\x1bWC\x1b[1;1H\x1b[K");
    assert_eq!(text(&t), "AB");
    assert_eq!(t.screen().protected_mode, ProtectedMode::Iso);
    let t = fed(10, 2, b"\x1bVAB\x1bWC\x1b[1;1H\x1b[5X");
    assert_eq!(text(&t), "AB");
    // ECH respects DEC protection? No: only ISO (Terminal.zig:3013).
    let t = fed(10, 2, b"\x1b[1\"qAB\x1b[1;1H\x1b[5X");
    assert_eq!(text(&t), "");
    // ED with protection.
    let t = fed(10, 2, b"\x1b[1\"qA\x1b[0\"qB\r\nC\x1b[?2J");
    assert_eq!(text(&t), "A");
    // DECALN ignores protection.
    let t = fed(2, 1, b"\x1bVA\x1b#8");
    assert_eq!(text(&t), "EE");
    assert!(!cell(&t, 0, 0).protected());
}

#[test]
fn tabstop_quirks() {
    // Tabstops.zig:96: TBC 0 toggles, so on a column with no stop it sets one.
    let t = fed(20, 2, b"\x1b[1;3H\x1b[0g\x1b[1;1H\t");
    assert_eq!(cursor(&t), (2, 0));
    // TBC needs exactly one parameter: bare CSI g is ignored (stream.zig:1652).
    let t = fed(20, 2, b"\x1b[1;9H\x1b[g\x1b[1;1H\t");
    assert_eq!(cursor(&t), (8, 0));
    let t = fed(20, 2, b"\x1b[1;9H\x1b[0g\x1b[1;1H\t");
    assert_eq!(cursor(&t), (16, 0));
    // TBC 3 clears all; HT then goes to the right margin.
    let t = fed(20, 2, b"\x1b[3g\t");
    assert_eq!(cursor(&t), (19, 0));
    // HTS, CTC 0 and DECST8C.
    let t = fed(20, 2, b"\x1b[3g\x1b[1;4H\x1bH\x1b[1;1H\t");
    assert_eq!(cursor(&t), (3, 0));
    let t = fed(20, 2, b"\x1b[3g\x1b[1;5H\x1b[W\x1b[1;1H\t");
    assert_eq!(cursor(&t), (4, 0));
    let t = fed(20, 2, b"\x1b[1;9H\x1b[2W\x1b[1;1H\t");
    assert_eq!(cursor(&t), (16, 0));
    let t = fed(20, 2, b"\x1b[5W\t");
    assert_eq!(cursor(&t), (19, 0));
    let t = fed(20, 2, b"\x1b[3g\x1b[?5W\t");
    assert_eq!(cursor(&t), (8, 0));
    // CHT 2.
    let t = fed(30, 2, b"\x1b[2I");
    assert_eq!(cursor(&t), (16, 0));
}

#[test]
fn margins() {
    // Terminal.zig:6612 setTopAndBottomMargin simple, 6700 top equal to bottom (no-op).
    let t = fed(5, 5, b"\x1b[2;3H\x1b[r");
    assert_eq!(cursor(&t), (0, 0));
    let t = fed(5, 5, b"\x1b[2;3H\x1b[2;2r");
    assert_eq!(cursor(&t), (2, 1));
    assert_eq!(t.region().top, 0);
    // DECSLRM needs mode 69 (Terminal.zig:6852); CSI s without it saves the cursor.
    let t = fed(5, 5, b"\x1b[2;3H\x1b[2;4s");
    assert_eq!(t.region().left, 0);
    assert_eq!(cursor(&t), (2, 1));
    let t = fed(5, 5, b"\x1b[2;3H\x1b[s\x1b[H\x1b[u");
    assert_eq!(cursor(&t), (2, 1));
    // With 69 set, CSI s is DECSLRM(0, 0): full width, home.
    let t = fed(5, 5, b"\x1b[?69h\x1b[2;4s\x1b[2;3H\x1b[s");
    assert_eq!((t.region().left, t.region().right), (0, 4));
    assert_eq!(cursor(&t), (0, 0));
    // Resetting 69 resets the left/right margins.
    let t = fed(5, 5, b"\x1b[?69h\x1b[2;4s\x1b[?69l");
    assert_eq!((t.region().left, t.region().right), (0, 4));
}

#[test]
fn save_restore_cursor() {
    // Terminal.zig:1655 / 1671: position, style, origin, charset and pending wrap.
    let mut t = fed(5, 5, b"\x1b[1mABCDE\x1b7\x1b[0m\x1b[H\x1b(0\x1b8X");
    assert_eq!(text(&t), "ABCDE\nX");
    assert!(t.screen().style(cell(&t, 0, 1).style_id()).flags.bold);
    // Nothing saved: home, default style, origin off.
    t.feed(b"\x1b[?6h\x1bc\x1b[3;3H\x1b8");
    assert_eq!(cursor(&t), (0, 0));
    assert!(!t.mode(Mode::Origin));
    // Mode 1048.
    let t = fed(5, 5, b"\x1b[2;2H\x1b[?1048h\x1b[H\x1b[?1048l");
    assert_eq!(cursor(&t), (1, 1));
}

#[test]
fn alt_screens() {
    // 1049: save, switch, clear, copy the cursor; leaving restores it (Terminal.zig:3775).
    let mut t = fed(5, 3, b"AB\x1b[?1049hX");
    assert_eq!(t.active(), Which::Alternate);
    assert_eq!(text(&t), "  X");
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.active(), Which::Primary);
    assert_eq!(text(&t), "AB");
    assert_eq!(cursor(&t), (2, 0));
    // 47 keeps the alt contents and copies the cursor; no clear.
    // The cursor is copied each way, so A lands after Q's column.
    let mut t = fed(5, 3, b"\x1b[?47hQ\x1b[?47lA\x1b[?47h");
    assert_eq!(text(&t), "Q");
    t.feed(b"\x1b[?47l");
    assert_eq!(text(&t), " A");
    // 1047 clears the alternate when leaving it.
    let mut t = fed(5, 3, b"\x1b[?1047hQ\x1b[?1047l\x1b[?47h");
    assert_eq!(text(&t), "");
    t.feed(b"\x1b[?47l");
    // The alternate keeps no scrollback.
    let t = fed(5, 2, b"\x1b[?1049h1\r\n2\r\n3\r\n4");
    assert_eq!(history_text(&t), "");
    assert_eq!(text(&t), "3\n4");
}

#[test]
fn full_reset() {
    // RIS, Terminal.zig:3885.
    let t = fed(5, 3, b"\x1b[?1049hA\x1b[2;3r\x1b[?6h\x1b[1m\x1bc");
    assert_eq!(t.active(), Which::Primary);
    assert!(t.alternate().is_none());
    assert_eq!(text(&t), "");
    assert_eq!((t.region().top, t.region().bottom), (0, 2));
    assert!(!t.mode(Mode::Origin));
    assert!(t.mode(Mode::Wraparound));
    assert_eq!(t.screen().cursor.style_id, 0);
    assert_eq!(all_text(&t), "");
}

#[test]
fn reports() {
    let mut t = term(80, 24);
    t.feed(b"\x1b[c");
    assert_eq!(replies(&mut t), b"\x1b[?62;22c");
    t.feed(b"\x1b[0c\x1bZ");
    assert_eq!(replies(&mut t), b"\x1b[?62;22c\x1b[?62;22c");
    t.feed(b"\x1b[>c");
    assert_eq!(replies(&mut t), b"\x1b[>1;0;0c");
    t.feed(b"\x1b[=c");
    assert_eq!(replies(&mut t), b"\x1bP!|00000000\x1b\\");
    t.feed(b"\x1b[5n");
    assert_eq!(replies(&mut t), b"\x1b[0n");
    t.feed(b"\x1b[3;7H\x1b[6n");
    assert_eq!(replies(&mut t), b"\x1b[3;7R");
    t.feed(b"\x1b[?6n\x1b[5;5n");
    assert_eq!(replies(&mut t), b"", "DECXCPR and two params get no reply");
    t.feed(b"\x1b[?996n");
    assert_eq!(replies(&mut t), b"", "no color scheme known");
    t.set_color_scheme(Some(crate::terminal::ColorScheme::Light));
    t.feed(b"\x1b[?996n");
    assert_eq!(replies(&mut t), b"\x1b[?997;2n");
    t.feed(b"\x1b[>q");
    assert_eq!(replies(&mut t), b"\x1bP>|libghostty\x1b\\");
    t.feed(b"\x1b[18t\x1b[14t\x1b[16t");
    assert_eq!(replies(&mut t), b"\x1b[8;24;80t", "no cell size: no pixel reports");
    t.set_cell_px(Some(crate::terminal::Pixels { width: 10, height: 20 }));
    t.feed(b"\x1b[14t\x1b[16t");
    assert_eq!(replies(&mut t), b"\x1b[4;480;800t\x1b[6;20;10t");
    t.feed(b"\x1b[?2048h");
    assert_eq!(replies(&mut t), b"\x1b[48;24;80;480;800t");
    t.set_title(b"hi");
    t.feed(b"\x1b[21t");
    assert_eq!(replies(&mut t), b"\x1b]lhi\x1b\\");
    t.feed(b"\x05");
    assert_eq!(replies(&mut t), b"", "no enquiry response");
    t.feed(b"\x1b[?1004h");
    assert_eq!(replies(&mut t), b"\x1b[I");
}

#[test]
fn decrqm() {
    let mut t = term(80, 24);
    t.feed(b"\x1b[?7$p\x1b[?6$p\x1b[?9999$p");
    assert_eq!(replies(&mut t), b"\x1b[?7;1$y\x1b[?6;2$y\x1b[?9999;0$y");
    // The ANSI form gets no reply (stream.zig:1859, a quirk kept).
    t.feed(b"\x1b[4$p");
    assert_eq!(replies(&mut t), b"");
    // Every DEC mode reports its default.
    for (m, n, ansi, default) in crate::terminal::modes::MODES {
        if ansi {
            continue;
        }
        let _ = m;
        t.feed(std::format!("\x1b[?{n}$p").as_bytes());
        let want = std::format!("\x1b[?{n};{}$y", if default { 1 } else { 2 });
        assert_eq!(replies(&mut t), want.as_bytes(), "mode {n}");
    }
}

#[test]
fn every_mode_sets_resets_saves_and_restores() {
    for (m, n, ansi, default) in crate::terminal::modes::MODES {
        // The screen-switching and cursor modes have side effects tested apart.
        let q = if ansi { "" } else { "?" };
        let mut t = term(80, 24);
        assert_eq!(t.mode(m), default, "mode {n} default");
        t.feed(std::format!("\x1b[{q}{n}h").as_bytes());
        let expect_on = !(m == Mode::Col132);
        assert_eq!(t.mode(m), expect_on, "mode {n} set");
        t.feed(std::format!("\x1b[{q}{n}l").as_bytes());
        assert!(!t.mode(m), "mode {n} reset");
        if !ansi {
            t.feed(std::format!("\x1b[?{n}h\x1b[?{n}s\x1b[?{n}l\x1b[?{n}r").as_bytes());
            assert_eq!(t.mode(m), expect_on, "mode {n} restored");
        }
    }
}

#[test]
fn deccolm() {
    // Terminal.zig:3509: only with mode 40.
    let t = fed(80, 5, b"A\x1b[?3h");
    assert_eq!(t.cols(), 80);
    assert!(!t.mode(Mode::Col132));
    let mut t = fed(80, 5, b"A\x1b[?40h\x1b[2;2H\x1b[?3h");
    assert_eq!(t.cols(), 132);
    assert_eq!(text(&t), "");
    assert_eq!(cursor(&t), (0, 0));
    let effects: StdVec<Effect> = t.take_effects().as_slice().to_vec();
    assert!(effects.contains(&Effect::ColumnMode { cols: 132 }));
}

#[test]
fn kitty_keyboard_stack() {
    let mut t = term(80, 24);
    t.feed(b"\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?0u");
    t.feed(b"\x1b[>1u\x1b[>3u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?3u");
    t.feed(b"\x1b[<u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?1u");
    t.feed(b"\x1b[=4;2u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?5u");
    t.feed(b"\x1b[=1;3u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?4u");
    t.feed(b"\x1b[=2u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?2u");
    t.feed(b"\x1b[>32u\x1b[=32u\x1b[=1;4u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?2u", "flags over 31 and mode 4 are invalid");
    t.feed(b"\x1b[<8u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?0u");
    // Eight pushes evict the oldest.
    for f in 1..=9 {
        t.feed(std::format!("\x1b[>{f}u").as_bytes());
    }
    t.feed(b"\x1b[<7u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?2u");
}

#[test]
fn title_stack() {
    let mut t = term(80, 24);
    t.set_title(b"one");
    t.feed(b"\x1b[22;0t");
    t.set_title(b"two");
    t.feed(b"\x1b[22;2t");
    t.set_title(b"three");
    t.feed(b"\x1b[23;0t");
    assert_eq!(t.title(), b"two");
    t.feed(b"\x1b[23;2;0t");
    assert_eq!(t.title(), b"one");
    t.feed(b"\x1b[23;0t");
    assert_eq!(t.title(), b"one", "an empty stack pops nothing");
    t.feed(b"\x1b[22;1t\x1b[22t");
    assert_eq!(t.title_stack_len(), 0, "icon titles and a bare 22 are not pushed");
}

#[test]
fn decscusr_and_xtshiftescape_and_xtmodkeys() {
    use crate::screen::CursorShape;
    let mut t = term(80, 24);
    t.feed(b"\x1b[3 q");
    assert_eq!(t.screen().cursor.shape, CursorShape::Underline);
    assert!(t.mode(Mode::CursorBlinking));
    t.feed(b"\x1b[6 q");
    assert_eq!(t.screen().cursor.shape, CursorShape::Bar);
    assert!(!t.mode(Mode::CursorBlinking));
    t.feed(b"\x1b[7 q");
    assert_eq!(t.screen().cursor.shape, CursorShape::Bar, "7 is invalid");
    t.feed(b"\x1b[ q");
    assert_eq!(t.screen().cursor.shape, CursorShape::Block);
    t.feed(b"\x1b[>1s");
    assert_eq!(t.flags().mouse_shift_capture, Some(true));
    t.feed(b"\x1b[>s");
    assert_eq!(t.flags().mouse_shift_capture, Some(false));
    t.feed(b"\x1b[>4;2m");
    assert!(t.flags().modify_other_keys_2);
    t.feed(b"\x1b[>4m");
    assert!(!t.flags().modify_other_keys_2);
}

#[test]
fn mouse_modes_set_flags() {
    use crate::terminal::{MouseEvents, MouseFormat};
    let mut t = term(80, 24);
    t.feed(b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(t.flags().mouse_event, MouseEvents::Button);
    assert_eq!(t.flags().mouse_format, MouseFormat::Sgr);
    t.feed(b"\x1b[?1000l\x1b[?1015l");
    assert_eq!(t.flags().mouse_event, MouseEvents::None);
    assert_eq!(t.flags().mouse_format, MouseFormat::X10);
}

#[test]
fn status_line_discards_prints() {
    let t = fed(10, 2, b"A\x1b[1$}B\x1b[0$}C");
    assert_eq!(text(&t), "AC");
}

#[test]
fn effects() {
    let mut t = fed(10, 2, b"\x07\x1b[?2026h\x1b[?2026l");
    let e: StdVec<Effect> = t.take_effects().as_slice().to_vec();
    assert_eq!(
        e,
        [Effect::Bell, Effect::SynchronizedOutput(true), Effect::SynchronizedOutput(false)]
    );
}

#[test]
fn printable_c0_and_del() {
    // Terminal.zig:12561: C0 above 0x0F (but ESC) print as cells, as DEL does.
    let t = fed(10, 2, b"\x10\x7f");
    assert_eq!(cell(&t, 0, 0).codepoint(), 0x10);
    assert_eq!(cell(&t, 1, 0).codepoint(), 0x7f);
}

#[test]
fn grapheme_mode_2027() {
    // Terminal.zig:4567 "print multicodepoint grapheme, mode 2027": the family emoji is one
    // wide cell with its codepoints.
    let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let t = fed(80, 2, std::format!("\x1b[?2027h{fam}").as_bytes());
    assert_eq!(cursor(&t), (2, 0));
    assert_eq!(text(&t), fam);
    assert_eq!(wide_at(&t, 0, 0), Wide::Wide);
    // Terminal.zig:4335 "disabled mode 2027": each part a cell, the ZWJ attached.
    let t = fed(80, 2, fam.as_bytes());
    assert_eq!(cursor(&t), (6, 0));
    // Terminal.zig:5104 "VS16 to make wide character with mode 2027".
    let t = fed(80, 2, "\x1b[?2027h\u{2764}\u{FE0F}".as_bytes());
    assert_eq!(cursor(&t), (2, 0));
    // Terminal.zig:4461 "VS16 doesn't make character with 2027 disabled".
    let t = fed(80, 2, "\u{2764}\u{FE0F}".as_bytes());
    assert_eq!(cursor(&t), (1, 0));
    assert_eq!(text(&t), "\u{2764}\u{FE0F}");
    // Terminal.zig:4503 "print invalid VS16 non-grapheme": dropped after a non-emoji.
    let t = fed(80, 2, "x\u{FE0F}".as_bytes());
    assert_eq!(text(&t), "x");
    // Terminal.zig:4770 "VS15 to make narrow character".
    let t = fed(80, 2, "\x1b[?2027h\u{2614}\u{FE0E}".as_bytes());
    assert_eq!(cursor(&t), (1, 0));
    // Terminal.zig:4966 "VS16 to make wide character on next line".
    let t = fed(3, 2, "\x1b[?2027hxx#\u{FE0F}".as_bytes());
    assert_eq!(text(&t), "xx\n#\u{FE0F}");
    assert_eq!(cursor(&t), (2, 1));
}

#[test]
fn resize_reflows_the_primary() {
    // Screen.zig resize: a soft-wrapped line unwraps when the columns grow.
    let mut t = fed(5, 3, b"ABCDEFGH");
    assert_eq!(text(&t), "ABCDE\nFGH");
    t.resize(10, 3);
    assert_eq!(text(&t), "ABCDEFGH");
    assert_eq!(cursor(&t), (8, 0));
    t.resize(4, 3);
    assert_eq!(text(&t), "ABCD\nEFGH");
    // Without DECAWM, no reflow: the columns are cut.
    let mut t = fed(5, 3, b"ABCDEFGH\x1b[?7l");
    t.resize(3, 3);
    assert_eq!(text(&t), "ABC\nFGH");
}
