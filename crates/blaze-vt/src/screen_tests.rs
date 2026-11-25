//! Screen tests: ghostty's Screen.zig resize tests ported as data (each names its line), run
//! through the terminal: `testWriteString`'s `\n` is `\r\n`, `cursorAbsolute(x, y)` is CUP,
//! `.reflow = false` is DECAWM reset before the resize (Terminal.zig:3537 reflows only with ?7).

use std::vec::Vec as StdVec;

use crate::page::Wide;
use crate::screen::{PromptRedraw, Resize, SemanticSet};
use crate::test_support::{all_text, cell, cursor, fed, term, text, wide_at};

struct ResizeCase {
    name: &'static str,
    at: &'static str,
    cols: u16,
    rows: u16,
    input: &'static str,
    reflow: bool,
    to: (u16, u16),
    text: &'static str,
    /// The whole screen, scrollback included, when ghostty's test dumps it.
    all: Option<&'static str>,
    cursor: Option<(usize, usize)>,
}

const fn case(
    name: &'static str,
    at: &'static str,
    size: (u16, u16),
    input: &'static str,
) -> ResizeCase {
    ResizeCase {
        name,
        at,
        cols: size.0,
        rows: size.1,
        input,
        reflow: true,
        to: size,
        text: "",
        all: None,
        cursor: None,
    }
}

fn run(cases: &[ResizeCase]) {
    let mut failed = StdVec::new();
    for c in cases {
        let mut t = fed(c.cols, c.rows, c.input.as_bytes());
        if !c.reflow {
            t.feed(b"\x1b[?7l");
        }
        t.resize(c.to.0, c.to.1);
        let got = text(&t);
        let all = all_text(&t);
        let at = cursor(&t);
        let bad =
            got != c.text || c.all.is_some_and(|a| a != all) || c.cursor.is_some_and(|x| x != at);
        if bad {
            failed.push(std::format!(
                "{} ({}):\n  text   {:?}\n  wanted {:?}\n  all    {:?}\n  wanted {:?}\n  cursor {:?}, wanted {:?}",
                c.name, c.at, got, c.text, all, c.all, at, c.cursor
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases failed:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

const LINES3: &str = "1ABCD\r\n2EFGH\r\n3IJKL";
const LINES5: &str = "1ABCD\r\n2EFGH\r\n3IJKL\r\n4ABCD\r\n5EFGH";
const DIGITS8: &str = "1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8";

#[test]
fn resize_without_reflow() {
    run(&[
        ResizeCase {
            reflow: false,
            to: (10, 10),
            text: "1ABCD\n2EFGH\n3IJKL",
            ..case("resize (no reflow) more rows", "Screen.zig:6393", (10, 3), LINES3)
        },
        ResizeCase {
            reflow: false,
            to: (10, 2),
            text: "2EFGH\n3IJKL",
            cursor: Some((5, 1)),
            ..case("resize (no reflow) less rows", "Screen.zig:6411", (10, 3), LINES3)
        },
        ResizeCase {
            reflow: false,
            to: (6, 2),
            text: "1ABCD",
            cursor: Some((5, 0)),
            ..case(
                "resize (no reflow) less rows trims blank lines",
                "Screen.zig:6434",
                (10, 3),
                "1ABCD\x1b[41m\x1b[2;1H\x1b[X\x1b[3;1H\x1b[X\x1b[0m\x1b[1;6H",
            )
        },
        ResizeCase {
            reflow: false,
            to: (10, 7),
            text: "1ABCD",
            cursor: Some((5, 0)),
            ..case(
                "resize (no reflow) more rows trims blank lines",
                "Screen.zig:6469",
                (10, 3),
                "1ABCD\x1b[41m\x1b[2;1H\x1b[X\x1b[3;1H\x1b[X\x1b[0m\x1b[1;6H",
            )
        },
        ResizeCase {
            reflow: false,
            to: (20, 3),
            text: "1ABCD\n2EFGH\n3IJKL",
            ..case("resize (no reflow) more cols", "Screen.zig:6504", (10, 3), LINES3)
        },
        ResizeCase {
            reflow: false,
            to: (4, 3),
            text: "1ABC\n2EFG\n3IJK",
            ..case("resize (no reflow) less cols", "Screen.zig:6521", (10, 3), LINES3)
        },
        ResizeCase {
            reflow: false,
            to: (7, 10),
            text: "1ABCD\n2EFGH\n3IJKL\n4ABCD\n5EFGH",
            ..case(
                "resize (no reflow) more rows with scrollback cursor end",
                "Screen.zig:6539",
                (7, 3),
                LINES5,
            )
        },
        ResizeCase {
            reflow: false,
            to: (7, 2),
            text: "4ABCD\n5EFGH",
            ..case(
                "resize (no reflow) less rows with scrollback",
                "Screen.zig:6556",
                (7, 3),
                LINES5,
            )
        },
        ResizeCase {
            reflow: false,
            to: (5, 2),
            text: "A\nB",
            cursor: Some((1, 1)),
            ..case(
                "resize (no reflow) less rows with empty trailing",
                "Screen.zig:6575",
                (5, 3),
                "1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\x1b[22J\x1b[HA\r\nB",
            )
        },
        ResizeCase {
            reflow: false,
            to: (2, 10),
            text: "1A\n2B\n3C\n4E\n5F\n6G",
            ..case(
                "resize (no reflow) more rows with soft wrapping",
                "Screen.zig:6599",
                (2, 3),
                "1A2B\r\n3C4E\r\n5F6G",
            )
        },
        ResizeCase {
            reflow: false,
            to: (8, 3),
            text: "6\n7\n8",
            all: Some("1\n2\n3\n4\n5\n6\n7\n8"),
            cursor: Some((1, 2)),
            ..case(
                "resize (no reflow) more cols with scrollback scrolled up",
                "Screen.zig:6786",
                (5, 3),
                DIGITS8,
            )
        },
        ResizeCase {
            reflow: false,
            to: (4, 3),
            text: "6\n7\n8",
            all: Some("1\n2\n3\n4\n5\n6\n7\n8"),
            cursor: Some((1, 2)),
            ..case(
                "resize (no reflow) less cols with scrollback scrolled up",
                "Screen.zig:6819",
                (5, 3),
                DIGITS8,
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "1AB\n2EF\n3IJ",
            cursor: Some((0, 0)),
            ..case(
                "resize less cols no reflow",
                "Screen.zig:7437",
                (5, 3),
                "1AB\r\n2EF\r\n3IJ\x1b[H",
            )
        },
    ]);
}

#[test]
fn resize_rows() {
    run(&[
        ResizeCase {
            to: (5, 10),
            text: "1ABCD\n2EFGH\n3IJKL",
            cursor: Some((4, 2)),
            ..case("resize more rows no scrollback", "Screen.zig:6640", (5, 3), LINES3)
        },
        ResizeCase {
            to: (5, 10),
            text: "1ABCD\n2EFGH\n3IJKL",
            cursor: Some((4, 2)),
            ..case("resize more rows with empty scrollback", "Screen.zig:6667", (5, 3), LINES3)
        },
        ResizeCase {
            to: (5, 10),
            text: "3IJKL\n4ABCD\n5EFGH",
            cursor: Some((0, 1)),
            ..case(
                "resize more rows with populated scrollback",
                "Screen.zig:6694",
                (5, 3),
                "1ABCD\r\n2EFGH\r\n3IJKL\r\n4ABCD\r\n5EFGH\x1b[2;1H",
            )
        },
        ResizeCase {
            to: (5, 1),
            text: "3IJKL",
            cursor: Some((0, 0)),
            ..case(
                "resize less rows no scrollback",
                "Screen.zig:7272",
                (5, 3),
                "1ABCD\r\n2EFGH\r\n3IJKL\x1b[H",
            )
        },
        ResizeCase {
            to: (5, 1),
            text: "3IJKL",
            cursor: Some((1, 0)),
            ..case(
                "resize less rows moving cursor",
                "Screen.zig:7303",
                (5, 3),
                "1ABCD\r\n2EFGH\r\n3IJKL\x1b[3;2H",
            )
        },
        ResizeCase {
            to: (5, 1),
            text: "3IJKL",
            all: Some("1ABCD\n2EFGH\n3IJKL"),
            ..case("resize less rows with empty scrollback", "Screen.zig:7343", (5, 3), LINES3)
        },
        ResizeCase {
            to: (5, 1),
            text: "5EFGH",
            all: Some("1ABCD\n2EFGH\n3IJKL\n4ABCD\n5EFGH"),
            ..case("resize less rows with populated scrollback", "Screen.zig:7366", (5, 3), LINES5)
        },
        ResizeCase {
            to: (5, 2),
            text: "4ABCD\n5EFGH",
            all: Some("00000\n1ABCD\n2EFGH\n3IJKL\n4ABCD\n5EFGH"),
            cursor: Some((4, 1)),
            ..case(
                "resize less rows with full scrollback",
                "Screen.zig:7397",
                (5, 3),
                "00000\r\n1ABCD\r\n2EFGH\r\n3IJKL\r\n4ABCD\r\n5EFGH",
            )
        },
    ]);
}

#[test]
fn resize_more_cols_reflows() {
    run(&[
        ResizeCase {
            to: (10, 3),
            text: "1ABCD\n2EFGH\n3IJKL",
            cursor: Some((4, 2)),
            ..case("resize more cols no reflow", "Screen.zig:6739", (5, 3), LINES3)
        },
        ResizeCase {
            to: (10, 3),
            text: "1ABCD2EFGH\n3IJKL",
            ..case("resize more cols perfect split", "Screen.zig:6768", (5, 3), "1ABCD2EFGH3IJKL")
        },
        ResizeCase {
            to: (10, 3),
            text: "1ABCD2EFGH\n3IJKL",
            cursor: Some((5, 0)),
            ..case(
                "resize more cols with reflow that fits full width",
                "Screen.zig:6907",
                (5, 3),
                "1ABCD2EFGH\r\n3IJKL\x1b[2;1H",
            )
        },
        ResizeCase {
            to: (10, 3),
            text: "1ABCD2EFGH\n3IJKL",
            cursor: Some((0, 1)),
            ..case(
                "resize more cols with reflow that ends in newline",
                "Screen.zig:6947",
                (6, 3),
                "1ABCD2EFGH\r\n3IJKL\x1b[3;1H",
            )
        },
        ResizeCase {
            to: (7, 3),
            text: "1ABCD2E\nFGH\n3IJKL",
            cursor: Some((5, 0)),
            ..case(
                "resize more cols with reflow that forces more wrapping",
                "Screen.zig:6992",
                (5, 3),
                "1ABCD2EFGH\r\n3IJKL\x1b[2;1H",
            )
        },
        ResizeCase {
            to: (15, 3),
            text: "1ABCD2EFGH3IJKL",
            cursor: Some((10, 0)),
            ..case(
                "resize more cols with reflow that unwraps multiple times",
                "Screen.zig:7033",
                (5, 3),
                "1ABCD2EFGH3IJKL\x1b[3;1H",
            )
        },
        ResizeCase {
            to: (10, 3),
            text: "2EFGH\n3IJKL\n4ABCD5EFGH",
            cursor: Some((5, 2)),
            ..case(
                "resize more cols with populated scrollback",
                "Screen.zig:7074",
                (5, 3),
                "1ABCD\r\n2EFGH\r\n3IJKL\r\n4ABCD5EFGH\x1b[3;1H",
            )
        },
        ResizeCase {
            to: (7, 3),
            all: Some("1ABC\n2DEF\n3ABC\n4DEF"),
            text: "2DEF\n3ABC\n4DEF",
            cursor: Some((2, 2)),
            ..case(
                "resize more cols with reflow",
                "Screen.zig:7198",
                (2, 3),
                "1ABC\r\n2DEF\r\n3ABC\r\n4DEF\x1b[3;1H",
            )
        },
        ResizeCase {
            to: (5, 10),
            text: "1A2B\n3C4D",
            cursor: Some((3, 1)),
            ..case(
                "resize more rows and cols with wrapping",
                "Screen.zig:7239",
                (2, 4),
                "1A2B\r\n3C4D",
            )
        },
    ]);
}

#[test]
fn resize_less_cols_reflows() {
    run(&[
        ResizeCase {
            to: (3, 3),
            text: "1AB\nCD",
            cursor: Some((1, 1)),
            ..case(
                "resize less cols with reflow but row space",
                "Screen.zig:7466",
                (5, 3),
                "1ABCD\x1b[1;5H",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "CD\n5EF\nGH",
            ..case(
                "resize less cols with reflow with trimmed rows",
                "Screen.zig:7504",
                (5, 3),
                "3IJKL\r\n4ABCD\r\n5EFGH",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "CD\n5EF\nGH",
            all: Some("3IJ\nKL\n4AB\nCD\n5EF\nGH"),
            ..case(
                "resize less cols with reflow with trimmed rows and scrollback",
                "Screen.zig:7528",
                (5, 3),
                "3IJKL\r\n4ABCD\r\n5EFGH",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "ABC\nD5E\nFGH",
            ..case(
                "resize less cols with reflow previously wrapped",
                "Screen.zig:7552",
                (5, 3),
                "3IJKL4ABCD5EFGH",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "3C\n4D\n5E",
            cursor: Some((1, 2)),
            ..case(
                "resize less cols with reflow and scrollback",
                "Screen.zig:7585",
                (5, 3),
                "1A\r\n2B\r\n3C\r\n4D\r\n5E\x1b[3;2H",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "CD5\nEFG\nH",
            all: Some("1AB\nCD2\nEFG\nH3I\nJKL\n4AB\nCD5\nEFG\nH"),
            cursor: Some((0, 2)),
            ..case(
                "resize less cols with reflow previously wrapped and scrollback",
                "Screen.zig:7618",
                (5, 3),
                "1ABCD2EFGH3IJKL4ABCD5EFGH\x1b[3;5H",
            )
        },
        ResizeCase {
            to: (3, 3),
            text: "",
            cursor: Some((0, 0)),
            ..case(
                "resize less cols with scrollback keeps cursor row",
                "Screen.zig:7672",
                (5, 3),
                "1A\r\n2B\r\n3C\r\n4D\r\n5E\x1b[22J\x1b[H",
            )
        },
        ResizeCase {
            to: (2, 10),
            text: "BC\nD\n2E\nFG\nH3\nIJ\nKL\n4M\nNO\nP",
            all: Some("1A\nBC\nD\n2E\nFG\nH3\nIJ\nKL\n4M\nNO\nP"),
            ..case(
                "resize more rows, less cols with reflow with scrollback",
                "Screen.zig:7701",
                (5, 3),
                "1ABCD\r\n2EFGH3IJKL\r\n4MNOP",
            )
        },
        ResizeCase {
            to: (1, 1),
            text: "",
            ..case(
                "resize less cols to eliminate wide char",
                "Screen.zig:7791",
                (2, 1),
                "\u{1F600}",
            )
        },
        ResizeCase {
            to: (2, 3),
            text: "x\n\u{1F600}",
            ..case("resize less cols to wrap wide char", "Screen.zig:7826", (3, 3), "x\u{1F600}")
        },
        ResizeCase {
            to: (1, 2),
            text: "",
            ..case(
                "resize less cols to eliminate wide char with row space",
                "Screen.zig:7865",
                (2, 2),
                "\u{1F600}",
            )
        },
        ResizeCase {
            to: (25, 7),
            text: "aaaaaaaaaaaaaaaaaaaaaaaaa\naaaaa",
            cursor: Some((5, 1)),
            ..case(
                "resize less cols reflows cursor after wrapped text",
                "Screen.zig:7898",
                (50, 7),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
        },
        ResizeCase {
            to: (5, 3),
            text: "abc",
            cursor: Some((4, 1)),
            ..case(
                "resize less cols reflows cursor after empty cells",
                "Screen.zig:7915",
                (10, 3),
                "abc\x1b[6C",
            )
        },
    ]);
}

#[test]
fn resize_wide_spacer_heads() {
    run(&[
        ResizeCase {
            to: (4, 2),
            text: "  \u{1F600}",
            ..case(
                "resize more cols with wide spacer head",
                "Screen.zig:7933",
                (3, 2),
                "  \u{1F600}",
            )
        },
        ResizeCase {
            to: (8, 2),
            text: "xxxyy\u{1F600}",
            ..case(
                "resize more cols with wide spacer head multiple lines",
                "Screen.zig:7986",
                (3, 3),
                "xxxyy\u{1F600}",
            )
        },
        ResizeCase {
            to: (3, 2),
            text: "xx\n\u{1F600}",
            ..case(
                "resize more cols requiring a wide spacer head",
                "Screen.zig:8037",
                (2, 2),
                "xx\u{1F600}",
            )
        },
    ]);
    // The cells, as ghostty's tests check them.
    let mut t = fed(3, 2, "  \u{1F600}".as_bytes());
    assert_eq!(wide_at(&t, 2, 0), Wide::SpacerHead);
    t.resize(4, 2);
    assert_eq!((wide_at(&t, 2, 0), wide_at(&t, 3, 0)), (Wide::Wide, Wide::SpacerTail));
    let mut t = fed(2, 2, "xx\u{1F600}".as_bytes());
    t.resize(3, 2);
    assert_eq!(wide_at(&t, 2, 0), Wide::SpacerHead);
    assert_eq!((wide_at(&t, 0, 1), wide_at(&t, 1, 1)), (Wide::Wide, Wide::SpacerTail));
    let mut t = fed(3, 3, "x\u{1F600}".as_bytes());
    t.resize(2, 3);
    assert_eq!(wide_at(&t, 1, 0), Wide::SpacerHead);
    assert!(t.screen().pages.row(t.screen().r(0)).wrap);
    let mut t = fed(2, 1, "\u{1F600}".as_bytes());
    t.resize(1, 1);
    assert_eq!((cell(&t, 0, 0).codepoint(), wide_at(&t, 0, 0)), (0, Wide::Narrow));
}

#[test]
fn resize_no_reflow_keeps_soft_wrap_flags() {
    // Screen.zig:6599.
    let mut t = fed(2, 3, b"1A2B\r\n3C4E\r\n5F6G\x1b[?7l");
    t.resize(2, 10);
    let s = t.screen();
    for y in 0..6 {
        assert_eq!(s.pages.row(s.r(y)).wrap, y % 2 == 0, "row {y}");
    }
}

#[test]
fn resize_more_rows_then_shrink_again() {
    // Screen.zig:7742.
    let mut t = fed(5, 3, b"1ABC");
    for (c, r) in [(5, 10), (5, 3), (5, 10)] {
        t.resize(c, r);
        assert_eq!(text(&t), "1ABC");
        assert_eq!(all_text(&t), "1ABC");
    }
}

#[test]
fn resize_more_cols_no_reflow_preserves_semantic_prompt() {
    // Screen.zig:6863.
    use crate::page::RowPrompt;
    let mut t = term(5, 3);
    t.feed(b"1ABCD\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"2EFGH");
    t.screen_mut().set_semantic_content(SemanticSet::Output);
    t.feed(b"\r\n3IJKL\x1b[?7l");
    t.resize(10, 3);
    assert_eq!(text(&t), "1ABCD\n2EFGH\n3IJKL");
    let s = t.screen();
    let prompts: StdVec<RowPrompt> = (0..3).map(|y| s.pages.row(s.r(y)).prompt).collect();
    assert_eq!(prompts, [RowPrompt::None, RowPrompt::Prompt, RowPrompt::None]);
}

fn screen_resize(
    t: &mut crate::terminal::Terminal<'_>,
    cols: usize,
    rows: usize,
    how: PromptRedraw,
) {
    t.screen_mut().resize(Resize { cols, rows, reflow: true, prompt_redraw: how });
}

#[test]
fn resize_more_cols_with_cursor_at_prompt() {
    // Screen.zig:8088: the shell redraws its prompt, so the prompt is cleared.
    let mut t = term(10, 3);
    t.feed(b"ABCDE\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"> ");
    t.screen_mut().set_semantic_content(SemanticSet::Input { clear_eol: true });
    t.feed(b"echo");
    assert_eq!(text(&t), "ABCDE\n> echo");
    screen_resize(&mut t, 20, 3, PromptRedraw::True);
    assert_eq!(cursor(&t), (6, 1));
    assert_eq!(text(&t), "ABCDE");
}

#[test]
fn resize_more_cols_with_cursor_not_at_prompt() {
    // Screen.zig:8128: the linefeed after input ended it (clear_eol), so nothing is cleared.
    let mut t = term(10, 3);
    t.feed(b"ABCDE\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"> ");
    t.screen_mut().set_semantic_content(SemanticSet::Input { clear_eol: true });
    t.feed(b"echo\r\noutput");
    screen_resize(&mut t, 20, 3, PromptRedraw::True);
    assert_eq!(cursor(&t), (6, 2));
    assert_eq!(text(&t), "ABCDE\n> echo\noutput");
}

#[test]
fn resize_with_prompt_redraw_last_clears_only_one_line() {
    // Screen.zig:8169.
    let mut t = term(10, 4);
    t.feed(b"ABCDE\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"> ");
    t.screen_mut().set_semantic_content(SemanticSet::Input { clear_eol: false });
    t.feed(b"hello\r\nworld");
    screen_resize(&mut t, 20, 4, PromptRedraw::Last);
    assert_eq!(text(&t), "ABCDE\n> hello");
}

#[test]
fn resize_with_prompt_redraw_last_multiline_prompt() {
    // Screen.zig:8208.
    let mut t = term(20, 5);
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"line1\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: true });
    t.feed(b"line2\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: true });
    t.feed(b"line3");
    screen_resize(&mut t, 30, 5, PromptRedraw::Last);
    assert_eq!(text(&t), "line1\nline2");
}

#[test]
fn resize_with_prompt_redraw_true_multiline_prompt_clears_all_of_it() {
    // Screen.zig resize with .true: from the prompt's first row down.
    let mut t = term(20, 5);
    t.feed(b"out\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: false });
    t.feed(b"line1\r\n");
    t.screen_mut().set_semantic_content(SemanticSet::Prompt { continuation: true });
    t.feed(b"line2");
    screen_resize(&mut t, 30, 5, PromptRedraw::True);
    assert_eq!(text(&t), "out");
}

#[test]
fn resize_keeps_the_saved_cursor_on_its_cell() {
    // The saved cursor is a pin too (Screen.zig resize tracks it).
    let mut t = fed(5, 3, b"ABCDEFG\x1b7\x1b[H");
    t.resize(10, 3);
    t.feed(b"\x1b8X");
    assert_eq!(text(&t), "ABCDEFGX");
}

#[test]
fn resize_keeps_graphemes_styles_and_links() {
    let mut t = term(4, 3);
    t.screen_mut().start_hyperlink(b"https://example.com", None);
    t.feed("\x1b[1mabce\u{301}fg".as_bytes());
    t.screen_mut().end_hyperlink();
    t.resize(8, 3);
    assert_eq!(text(&t), "abce\u{301}fg");
    let s = t.screen();
    assert!(cell(&t, 3, 0).has_grapheme());
    assert!(s.style(cell(&t, 4, 0).style_id()).flags.bold);
    assert_ne!(s.cell_link(cell(&t, 5, 0)), 0);
    t.resize(3, 3);
    assert_eq!(text(&t), "abc\ne\u{301}fg");
    assert!(cell(&t, 0, 1).has_grapheme());
}
