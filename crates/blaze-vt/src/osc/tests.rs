//! OSC tests: ghostty's parser tests, ported by name (one file per large ghostty parser file),
//! the stream handler's colour tests for [`run_colors`], and one test per vt-inventory §7 row.

use super::*;
use crate::color::{DEFAULT_PALETTE, DynamicRgb};
use std::vec;
use std::vec::Vec;

mod context_signal;
mod kitty;
mod osc9;
mod semantic_prompt;

/// Parse with ST, as ghostty's tests end with `p.end('\x1b')` or `p.end(null)`.
pub(super) fn p(s: &[u8]) -> Option<Command<'_>> {
    parse(s, Terminator::St)
}

pub(super) fn reqs(cmd: &Command<'_>) -> Vec<Request> {
    match cmd {
        Command::ColorOperation(op) => op.requests().collect(),
        other => panic!("not a colour operation: {other:?}"),
    }
}

/// color.zig's tests call `parseColor(op, body)` directly.
fn parse_color(op: Operation, body: &[u8]) -> Vec<Request> {
    ColorOperation { op, data: body, terminator: Terminator::St }.requests().collect()
}

fn kitty_reqs(cmd: &Command<'_>) -> Vec<KittyRequest> {
    match cmd {
        Command::KittyColorProtocol(k) => k.requests().collect(),
        other => panic!("not OSC 21: {other:?}"),
    }
}

const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };
const BLUE: Rgb = Rgb { r: 0, g: 0, b: 255 };

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

fn repeat(prefix: &[u8], b: u8, n: usize) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.resize(prefix.len() + n, b);
    v
}

// ---- change_window_title.zig, change_window_icon.zig

// change_window_title.zig:23 "OSC 0: change_window_title"
#[test]
fn osc_0_change_window_title() {
    assert_eq!(p(b"0;ab"), Some(Command::ChangeWindowTitle(b"ab")));
}

// change_window_title.zig:36 "OSC 0: longer than buffer"
#[test]
fn osc_0_longer_than_buffer() {
    assert_eq!(p(&repeat(b"0;", b'a', MAX_BUF + 2)), None);
}

// change_window_title.zig:47 "OSC 0: one shorter than buffer length"
#[test]
fn osc_0_one_shorter_than_buffer_length() {
    let input = repeat(b"0;", b'a', MAX_BUF - 1);
    assert_eq!(p(&input), Some(Command::ChangeWindowTitle(&input[2..])));
}

// change_window_title.zig:62 "OSC 0: exactly at buffer length"
#[test]
fn osc_0_exactly_at_buffer_length() {
    // No room for ghostty's NUL.
    assert_eq!(p(&repeat(b"0;", b'a', MAX_BUF)), None);
}

// change_window_title.zig:75 "OSC 2: change_window_title with 2"
#[test]
fn osc_2_change_window_title_with_2() {
    assert_eq!(p(b"2;ab"), Some(Command::ChangeWindowTitle(b"ab")));
}

// change_window_title.zig:88 "OSC 2: change_window_title with utf8"
#[test]
fn osc_2_change_window_title_with_utf8() {
    assert_eq!(
        p(b"2;\xE2\x80\x94 \xE2\x80\x90"),
        Some(Command::ChangeWindowTitle("— ‐".as_bytes()))
    );
}

// change_window_title.zig:110 "OSC 2: change_window_title empty"
#[test]
fn osc_2_change_window_title_empty() {
    assert_eq!(p(b"2;"), Some(Command::ChangeWindowTitle(b"")));
}

// change_window_icon.zig:22 "OSC 1: change_window_icon"
#[test]
fn osc_1_change_window_icon() {
    assert_eq!(p(b"1;ab"), Some(Command::ChangeWindowIcon(b"ab")));
}

// ---- clipboard_operation.zig

// clipboard_operation.zig:50 "OSC 52: get/set clipboard"
#[test]
fn osc_52_get_set_clipboard() {
    assert_eq!(p(b"52;s;?"), Some(Command::ClipboardContents { kind: b's', data: b"?" }));
}

// clipboard_operation.zig:64 "OSC 52: get/set clipboard (optional parameter)"
#[test]
fn osc_52_get_set_clipboard_optional_parameter() {
    assert_eq!(p(b"52;;?"), Some(Command::ClipboardContents { kind: b'c', data: b"?" }));
}

// clipboard_operation.zig:78 "OSC 52: get/set clipboard with allocator"
// (Ours always captures OSC 52 unbounded; the same result.)
#[test]
fn osc_52_get_set_clipboard_with_allocator() {
    assert_eq!(p(b"52;s;?"), Some(Command::ClipboardContents { kind: b's', data: b"?" }));
}

// clipboard_operation.zig:93 "OSC 52: clear clipboard"
#[test]
fn osc_52_clear_clipboard() {
    assert_eq!(p(b"52;;"), Some(Command::ClipboardContents { kind: b'c', data: b"" }));
}

// ---- report_pwd.zig, mouse_shape.zig, rxvt_extension.zig

// report_pwd.zig:25 "OSC 7: report pwd"
#[test]
fn osc_7_report_pwd() {
    assert_eq!(p(b"7;file:///tmp/example"), Some(Command::ReportPwd(b"file:///tmp/example")));
}

// report_pwd.zig:38 "OSC 7: report pwd empty"
#[test]
fn osc_7_report_pwd_empty() {
    assert_eq!(p(b"7;"), Some(Command::ReportPwd(b"")));
}

// mouse_shape.zig:28 "OSC 22: pointer cursor"
#[test]
fn osc_22_pointer_cursor() {
    assert_eq!(p(b"22;pointer"), Some(Command::MouseShape(b"pointer")));
}

// mouse.zig:161 "cursor shape from string"
#[test]
fn cursor_shape_from_string() {
    assert_eq!(MouseShape::from_name(b"default"), Some(MouseShape::Default));
    // Beyond ghostty's test: aliases, case sensitivity, unknown.
    assert_eq!(MouseShape::from_name(b"left_ptr"), Some(MouseShape::Default));
    assert_eq!(MouseShape::from_name(b"dnd-no-drop"), Some(MouseShape::NoDrop));
    assert_eq!(MouseShape::from_name(b"bottom_left_corner"), Some(MouseShape::SwResize));
    assert_eq!(MouseShape::from_name(b"zoom-out"), Some(MouseShape::ZoomOut));
    assert_eq!(MouseShape::from_name(b"Pointer"), None);
    assert_eq!(MouseShape::from_name(b"nope"), None);
}

// rxvt_extension.zig:47 "OSC: OSC 777 show desktop notification with title"
#[test]
fn osc_777_show_desktop_notification_with_title() {
    assert_eq!(
        p(b"777;notify;Title;Body"),
        Some(Command::ShowDesktopNotification { title: b"Title", body: b"Body" })
    );
    // Beyond ghostty's test: only `notify`, and the title needs its `;`.
    assert_eq!(p(b"777;other;Title;Body"), None);
    assert_eq!(p(b"777;notify;Title"), None);
}

// ---- hyperlink.zig

// hyperlink.zig:59 "OSC 8: hyperlink"
#[test]
fn osc_8_hyperlink() {
    assert_eq!(
        p(b"8;;http://example.com"),
        Some(Command::HyperlinkStart { id: None, uri: b"http://example.com" })
    );
}

// hyperlink.zig:72 "OSC 8: hyperlink with id set"
#[test]
fn osc_8_hyperlink_with_id_set() {
    assert_eq!(
        p(b"8;id=foo;http://example.com"),
        Some(Command::HyperlinkStart { id: Some(b"foo"), uri: b"http://example.com" })
    );
}

// hyperlink.zig:86 "OSC 8: hyperlink with empty id"
#[test]
fn osc_8_hyperlink_with_empty_id() {
    assert_eq!(
        p(b"8;id=;http://example.com"),
        Some(Command::HyperlinkStart { id: None, uri: b"http://example.com" })
    );
}

// hyperlink.zig:100 "OSC 8: hyperlink with incomplete key"
#[test]
fn osc_8_hyperlink_with_incomplete_key() {
    assert_eq!(
        p(b"8;id;http://example.com"),
        Some(Command::HyperlinkStart { id: None, uri: b"http://example.com" })
    );
}

// hyperlink.zig:114 "OSC 8: hyperlink with empty key"
#[test]
fn osc_8_hyperlink_with_empty_key() {
    assert_eq!(
        p(b"8;=value;http://example.com"),
        Some(Command::HyperlinkStart { id: None, uri: b"http://example.com" })
    );
}

// hyperlink.zig:128 "OSC 8: hyperlink with empty key and id"
#[test]
fn osc_8_hyperlink_with_empty_key_and_id() {
    assert_eq!(
        p(b"8;=value:id=foo;http://example.com"),
        Some(Command::HyperlinkStart { id: Some(b"foo"), uri: b"http://example.com" })
    );
}

// hyperlink.zig:142 "OSC 8: hyperlink with empty uri"
#[test]
fn osc_8_hyperlink_with_empty_uri() {
    assert_eq!(p(b"8;id=foo;"), None);
}

// hyperlink.zig:154 "OSC 8: hyperlink end"
#[test]
fn osc_8_hyperlink_end() {
    assert_eq!(p(b"8;;"), Some(Command::HyperlinkEnd));
}

// Beyond ghostty's tests: a later id wins, a doubled separator hides the key after it, other
// keys are ignored, and a `:` in the URI is the URI's.
#[test]
fn osc_8_hyperlink_params() {
    let start = |id, uri| Some(Command::HyperlinkStart { id, uri });
    assert_eq!(p(b"8;id=a:id=b;u"), start(Some(&b"b"[..]), &b"u"[..]));
    assert_eq!(p(b"8;x=1:id=a;u"), start(Some(&b"a"[..]), &b"u"[..]));
    assert_eq!(p(b"8;x=1::id=a;u"), start(None, &b"u"[..]));
    assert_eq!(p(b"8;id=a;http://h:80/x"), start(Some(&b"a"[..]), &b"http://h:80/x"[..]));
    assert_eq!(p(b"8"), None);
    assert_eq!(p(b"8;nouri"), None);
}

// ---- iterm2.zig

fn iterm(s: &[u8]) -> Option<Command<'_>> {
    p(s)
}

// iterm2.zig:200 "OSC: 1337: test valid unimplemented key with no value"
#[test]
fn osc_1337_valid_unimplemented_key_with_no_value() {
    assert_eq!(iterm(b"1337;SetBadgeFormat"), None);
}

// iterm2.zig:212 "OSC: 1337: test valid unimplemented key with empty value"
#[test]
fn osc_1337_valid_unimplemented_key_with_empty_value() {
    assert_eq!(iterm(b"1337;SetBadgeFormat="), None);
}

// iterm2.zig:224 "OSC: 1337: test valid unimplemented key with non-empty value"
#[test]
fn osc_1337_valid_unimplemented_key_with_non_empty_value() {
    assert_eq!(iterm(b"1337;SetBadgeFormat=abc123"), None);
}

// iterm2.zig:236 "OSC: 1337: test valid key with lower case and with no value"
#[test]
fn osc_1337_valid_key_with_lower_case_and_with_no_value() {
    assert_eq!(iterm(b"1337;setbadgeformat"), None);
}

// iterm2.zig:248 "OSC: 1337: test valid key with lower case and with empty value"
#[test]
fn osc_1337_valid_key_with_lower_case_and_with_empty_value() {
    assert_eq!(iterm(b"1337;setbadgeformat="), None);
}

// iterm2.zig:260 "OSC: 1337: test valid key with lower case and with non-empty value"
#[test]
fn osc_1337_valid_key_with_lower_case_and_with_non_empty_value() {
    assert_eq!(iterm(b"1337;setbadgeformat=abc123"), None);
}

// iterm2.zig:272 "OSC: 1337: test invalid key with no value"
#[test]
fn osc_1337_invalid_key_with_no_value() {
    assert_eq!(iterm(b"1337;BobrKurwa"), None);
}

// iterm2.zig:284 "OSC: 1337: test invalid key with empty value"
#[test]
fn osc_1337_invalid_key_with_empty_value() {
    assert_eq!(iterm(b"1337;BobrKurwa="), None);
}

// iterm2.zig:296 "OSC: 1337: test invalid key with non-empty value"
#[test]
fn osc_1337_invalid_key_with_non_empty_value() {
    assert_eq!(iterm(b"1337;BobrKurwa=abc123"), None);
}

// iterm2.zig:308 "OSC: 1337: test Copy with no value"
#[test]
fn osc_1337_copy_with_no_value() {
    assert_eq!(iterm(b"1337;Copy"), None);
}

// iterm2.zig:320 "OSC: 1337: test Copy with empty value"
#[test]
fn osc_1337_copy_with_empty_value() {
    assert_eq!(iterm(b"1337;Copy="), None);
}

// iterm2.zig:332 "OSC: 1337: test Copy with only prefix colon"
#[test]
fn osc_1337_copy_with_only_prefix_colon() {
    assert_eq!(iterm(b"1337;Copy=:"), None);
}

// iterm2.zig:344 "OSC: 1337: test Copy with question mark"
#[test]
fn osc_1337_copy_with_question_mark() {
    assert_eq!(iterm(b"1337;Copy=:?"), None);
}

// iterm2.zig:356 "OSC: 1337: test Copy with non-empty value that is invalid base64"
// Skipped in ghostty (base64 is not checked); ours likewise passes the bytes on.
#[test]
fn osc_1337_copy_with_non_empty_value_that_is_invalid_base64() {
    assert_eq!(
        iterm(b"1337;Copy=:abc123"),
        Some(Command::ClipboardContents { kind: b'c', data: b"abc123" })
    );
}

// iterm2.zig:372 "OSC: 1337: test Copy with non-empty value that is valid base64 but not prefixed with a colon"
#[test]
fn osc_1337_copy_valid_base64_not_prefixed_with_a_colon() {
    assert_eq!(iterm(b"1337;Copy=YWJjMTIz"), None);
}

// iterm2.zig:384 "OSC: 1337: test Copy with non-empty value that is valid base64"
#[test]
fn osc_1337_copy_with_non_empty_value_that_is_valid_base64() {
    assert_eq!(
        iterm(b"1337;Copy=:YWJjMTIz"),
        Some(Command::ClipboardContents { kind: b'c', data: b"YWJjMTIz" })
    );
    // Keys are case-insensitive.
    assert_eq!(
        iterm(b"1337;copy=:YWJjMTIz"),
        Some(Command::ClipboardContents { kind: b'c', data: b"YWJjMTIz" })
    );
}

// iterm2.zig:399 "OSC: 1337: test CurrentDir with no value"
#[test]
fn osc_1337_current_dir_with_no_value() {
    assert_eq!(iterm(b"1337;CurrentDir"), None);
}

// iterm2.zig:411 "OSC: 1337: test CurrentDir with empty value"
#[test]
fn osc_1337_current_dir_with_empty_value() {
    assert_eq!(iterm(b"1337;CurrentDir="), None);
}

// iterm2.zig:423 "OSC: 1337: test CurrentDir with non-empty value"
#[test]
fn osc_1337_current_dir_with_non_empty_value() {
    assert_eq!(iterm(b"1337;CurrentDir=abc123"), Some(Command::ReportPwd(b"abc123")));
}

// ---- kitty_color.zig

const KITTY_INPUT: &[u8] = b"21;foreground=?;background=rgb:f0/f8/ff;cursor=aliceblue;cursor_text;visual_bell=;selection_foreground=#xxxyyzz;selection_background=?;selection_background=#aabbcc;2=?;3=rgbi:1.0/1.0/1.0";

// kitty_color.zig:80 "OSC 21: kitty color protocol"
#[test]
fn osc_21_kitty_color_protocol() {
    use KittyRequest as R;
    use KittySpecial as S;
    let k = KittyKey::Special;
    let cmd = p(KITTY_INPUT).unwrap();
    assert_eq!(
        kitty_reqs(&cmd),
        [
            R::Query(k(S::Foreground)),
            R::Set { key: k(S::Background), color: rgb(0xf0, 0xf8, 0xff) },
            R::Set { key: k(S::Cursor), color: rgb(0xf0, 0xf8, 0xff) },
            R::Reset(k(S::CursorText)),
            R::Reset(k(S::VisualBell)),
            R::Query(k(S::SelectionBackground)),
            R::Set { key: k(S::SelectionBackground), color: rgb(0xaa, 0xbb, 0xcc) },
            R::Query(KittyKey::Palette(2)),
            R::Set { key: KittyKey::Palette(3), color: rgb(0xff, 0xff, 0xff) },
        ]
    );
}

// kitty_color.zig:152 "OSC 21: kitty color protocol without allocator"
// ghostty refuses OSC 21 when its parser has no allocator; ours always has the payload.
#[test]
fn osc_21_kitty_color_protocol_without_allocator() {
    let cmd = p(b"21;foreground=?").unwrap();
    assert_eq!(
        kitty_reqs(&cmd),
        [KittyRequest::Query(KittyKey::Special(KittySpecial::Foreground))]
    );
}

// kitty_color.zig:163 "OSC 21: kitty color protocol double reset"
// (ghostty checks its parser's reset is idempotent; parsing twice is ours.)
#[test]
fn osc_21_kitty_color_protocol_double_reset() {
    assert_eq!(p(KITTY_INPUT), p(KITTY_INPUT));
    assert!(matches!(p(KITTY_INPUT), Some(Command::KittyColorProtocol(_))));
}

// kitty_color.zig:179 "OSC 21: kitty color protocol reset after invalid"
// (ghostty: after a reset, a non-digit makes the parser invalid.)
#[test]
fn osc_21_kitty_color_protocol_reset_after_invalid() {
    assert!(matches!(p(KITTY_INPUT), Some(Command::KittyColorProtocol(_))));
    assert_eq!(p(b"X"), None);
}

// kitty_color.zig:200 "OSC 21: kitty color protocol no key"
#[test]
fn osc_21_kitty_color_protocol_no_key() {
    assert_eq!(kitty_reqs(&p(b"21;").unwrap()), []);
}

// The request limit: 2 × (255 + 8) items, checked before each `;` piece. OSC 21 captures into
// the fixed buffer (osc.zig:673), so the shortest item, a reset, is used to reach it.
#[test]
fn osc_21_request_limit() {
    let mut ok = b"21".to_vec();
    for _ in 0..KittyKey::MAX * 2 {
        ok.extend_from_slice(b";1=");
    }
    assert!(ok.len() - 3 <= MAX_BUF);
    assert_eq!(kitty_reqs(&p(&ok).unwrap()).len(), 526);
    let mut over = ok.clone();
    over.extend_from_slice(b";x");
    assert_eq!(p(&over), None);
    // Skipped pieces do not count.
    let mut skipped = b"21".to_vec();
    for _ in 0..600 {
        skipped.extend_from_slice(b";x");
    }
    assert_eq!(kitty_reqs(&p(&skipped).unwrap()).len(), 0);
}

// ---- osc/parsers/color.zig

// color.zig:102 "OSC 4: empty param"
// ghostty returns null only because the test's parser has no allocator; with one, "4;;" is
// an operation with no requests, which is ours.
#[test]
fn osc_4_empty_param() {
    let cmd = p(b"4;;").unwrap();
    assert_eq!(reqs(&cmd), []);
}

// color.zig:357 "OSC 4:"
#[test]
fn osc_4() {
    for idx in 0..=255u8 {
        let set = Request::Set { target: Target::Palette(idx), color: RED };
        assert_eq!(parse_color(Operation::Palette, std::format!("{idx};red").as_bytes()), [set]);
        assert_eq!(
            parse_color(Operation::Palette, std::format!("{idx};?").as_bytes()),
            [Request::Query(Target::Palette(idx))]
        );
        assert_eq!(parse_color(Operation::Palette, std::format!("{idx};red;").as_bytes()), [set]);
        assert_eq!(parse_color(Operation::Palette, std::format!("{idx};red ").as_bytes()), [set]);
    }
    for (i, special) in Special::ALL.iter().enumerate() {
        assert_eq!(
            parse_color(Operation::Palette, std::format!("{};red", 256 + i).as_bytes()),
            [Request::Set { target: Target::Special(*special), color: RED }]
        );
    }
}

// color.zig:479 "OSC 5:"
#[test]
fn osc_5() {
    for (i, special) in Special::ALL.iter().enumerate() {
        assert_eq!(
            parse_color(Operation::Special, std::format!("{i};red").as_bytes()),
            [Request::Set { target: Target::Special(*special), color: RED }]
        );
    }
}

// color.zig:511 "OSC 4: multiple requests"
#[test]
fn osc_4_multiple_requests() {
    assert_eq!(
        parse_color(Operation::Palette, b"0;red;1;blue"),
        [
            Request::Set { target: Target::Palette(0), color: RED },
            Request::Set { target: Target::Palette(1), color: BLUE }
        ]
    );
    assert_eq!(
        parse_color(Operation::Palette, b"0;red;0;blue"),
        [
            Request::Set { target: Target::Palette(0), color: RED },
            Request::Set { target: Target::Palette(0), color: BLUE }
        ]
    );
}

// color.zig:567 "OSC 104:"
#[test]
fn osc_104() {
    for idx in 0..=255u8 {
        assert_eq!(
            parse_color(Operation::ResetPalette, std::format!("{idx}").as_bytes()),
            [Request::Reset(Target::Palette(idx))]
        );
    }
    for (i, special) in Special::ALL.iter().enumerate() {
        assert_eq!(
            parse_color(Operation::ResetPalette, std::format!("{}", 256 + i).as_bytes()),
            [Request::Reset(Target::Special(*special))]
        );
    }
}

// color.zig:618 "OSC 104: empty index"
#[test]
fn osc_104_empty_index() {
    assert_eq!(
        parse_color(Operation::ResetPalette, b"0;;1"),
        [Request::Reset(Target::Palette(0)), Request::Reset(Target::Palette(1))]
    );
}

// color.zig:635 "OSC 104: invalid index"
#[test]
fn osc_104_invalid_index() {
    assert_eq!(
        parse_color(Operation::ResetPalette, b"ffff;1"),
        [Request::Reset(Target::Palette(1))]
    );
}

// color.zig:648 "OSC 104: reset all"
#[test]
fn osc_104_reset_all() {
    assert_eq!(parse_color(Operation::ResetPalette, b""), [Request::ResetPalette]);
}

// color.zig:661 "OSC 105: reset all"
#[test]
fn osc_105_reset_all() {
    assert_eq!(parse_color(Operation::ResetSpecial, b""), [Request::ResetSpecial]);
}

// color.zig:675 "OSC 10: … OSC 19: dynamic"
#[test]
fn osc_10_to_19_dynamic() {
    for d in Dynamic::ALL {
        assert_eq!(
            parse_color(Operation::Dynamic(d), b"red"),
            [Request::Set { target: Target::Dynamic(d), color: RED }]
        );
        // And through the prefix.
        let input = std::format!("{};red", d.code());
        assert_eq!(
            reqs(&p(input.as_bytes()).unwrap()),
            [Request::Set { target: Target::Dynamic(d), color: RED }]
        );
    }
}

// color.zig:703 "OSC 10: … OSC 19: dynamic multiple"
#[test]
fn osc_10_to_19_dynamic_multiple() {
    assert_eq!(
        parse_color(Operation::Dynamic(Dynamic::Background), b"red;blue"),
        [
            Request::Set { target: Target::Dynamic(Dynamic::Background), color: RED },
            Request::Set { target: Target::Dynamic(Dynamic::Cursor), color: BLUE },
        ]
    );
}

// color.zig:735 "OSC 110: … OSC 119: reset dynamic"
#[test]
fn osc_110_to_119_reset_dynamic() {
    for d in Dynamic::ALL {
        let op = Operation::ResetDynamic(d);
        assert_eq!(parse_color(op, b""), [Request::Reset(Target::Dynamic(d))]);
        assert_eq!(parse_color(op, b";"), [Request::Reset(Target::Dynamic(d))]);
        assert_eq!(parse_color(op, b" "), []);
        let input = std::format!("1{}", d.code());
        assert_eq!(reqs(&p(input.as_bytes()).unwrap()), [Request::Reset(Target::Dynamic(d))]);
    }
}

// ---- stream_terminal.zig's colour handler tests, against run_colors

fn run(colors: &mut Colors, input: &[u8]) -> (Vec<u8>, ColorEffect) {
    // `input` is a whole OSC: ESC ] payload terminator.
    let body = input.strip_prefix(b"\x1b]").unwrap();
    let (payload, term) = if let Some(b) = body.strip_suffix(b"\x1b\\") {
        (b, Terminator::St)
    } else {
        (body.strip_suffix(b"\x07").unwrap(), Terminator::Bel)
    };
    let mut out = Vec::new();
    let fx = match parse(payload, term) {
        Some(cmd) => run_colors(&cmd, colors, &mut |b: &[u8]| out.extend_from_slice(b)),
        None => ColorEffect::default(),
    };
    (out, fx)
}

// stream_terminal.zig:1165 "OSC 4 set and reset palette"
#[test]
fn osc_4_set_and_reset_palette() {
    let mut c = Colors::default();
    let default_0 = c.palette.original[0];
    run(&mut c, b"\x1b]4;0;rgb:ff/00/00\x1b\\");
    assert_eq!(c.palette.current[0], RED);
    assert!(c.palette.mask.is_set(0));
    run(&mut c, b"\x1b]104;0\x1b\\");
    assert_eq!(c.palette.current[0], default_0);
    assert!(!c.palette.mask.is_set(0));
}

// stream_terminal.zig:1188 "OSC 104 reset all palette colors"
#[test]
fn osc_104_reset_all_palette_colors() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]4;0;rgb:ff/00/00\x1b\\");
    run(&mut c, b"\x1b]4;1;rgb:00/ff/00\x1b\\");
    run(&mut c, b"\x1b]4;2;rgb:00/00/ff\x1b\\");
    assert!((0..3).all(|i| c.palette.mask.is_set(i)));
    run(&mut c, b"\x1b]104\x1b\\");
    assert_eq!(c.palette.current, c.palette.original);
    assert!((0..3).all(|i| !c.palette.mask.is_set(i)));
}

// stream_terminal.zig:1213 "OSC 10 set and reset foreground color"
#[test]
fn osc_10_set_and_reset_foreground_color() {
    let mut c = Colors::default();
    assert_eq!(c.foreground.get(), None);
    run(&mut c, b"\x1b]10;rgb:ff/00/00\x1b\\");
    assert_eq!(c.foreground.get(), Some(RED));
    run(&mut c, b"\x1b]110\x1b\\");
    assert_eq!(c.foreground.get(), None);
}

// stream_terminal.zig:1235 "OSC 11 set and reset background color"
#[test]
fn osc_11_set_and_reset_background_color() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]11;rgb:00/ff/00\x1b\\");
    assert_eq!(c.background.get(), Some(rgb(0, 255, 0)));
    run(&mut c, b"\x1b]111\x1b\\");
    assert_eq!(c.background.get(), None);
}

// stream_terminal.zig:1254 "OSC 12 set and reset cursor color"
#[test]
fn osc_12_set_and_reset_cursor_color() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]12;rgb:00/00/ff\x1b\\");
    assert_eq!(c.cursor.get(), Some(BLUE));
    run(&mut c, b"\x1b]112\x1b\\");
    assert_eq!(c.cursor.get(), None);
}

// stream_terminal.zig:1273 "OSC color query responses"
#[test]
fn osc_color_query_responses() {
    let mut c = Colors::default();
    assert_eq!(run(&mut c, b"\x1b]10;?\x1b\\").0, b"");
    assert_eq!(run(&mut c, b"\x1b]11;?\x1b\\").0, b"");
    assert_eq!(
        run(&mut c, b"\x1b]4;2;rgb:12/34/56;2;?\x1b\\").0,
        b"\x1b]4;2;rgb:1212/3434/5656\x1b\\"
    );
    run(&mut c, b"\x1b]10;rgb:01/02/03\x1b\\");
    run(&mut c, b"\x1b]11;rgb:04/05/06\x1b\\");
    run(&mut c, b"\x1b]12;rgb:07/08/09\x1b\\");
    assert_eq!(
        run(&mut c, b"\x1b]10;?;?;?\x1b\\").0,
        b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]12;rgb:0707/0808/0909\x1b\\"
    );
    run(&mut c, b"\x1b]112\x1b\\");
    // The cursor falls back to the foreground; the reply ends as the request did.
    assert_eq!(run(&mut c, b"\x1b]12;?\x07").0, b"\x1b]12;rgb:0101/0202/0303\x07");
}

// stream_terminal.zig:1330 "kitty color protocol set palette"
#[test]
fn kitty_color_protocol_set_palette() {
    let mut c = Colors::default();
    let (_, fx) = run(&mut c, b"\x1b]21;5=rgb:ff/00/ff\x1b\\");
    assert_eq!(c.palette.current[5], rgb(0xff, 0, 0xff));
    assert!(c.palette.mask.is_set(5));
    assert!(fx.palette);
}

// stream_terminal.zig:1346 "kitty color protocol reset palette"
#[test]
fn kitty_color_protocol_reset_palette() {
    let mut c = Colors::default();
    let original = c.palette.original[7];
    run(&mut c, b"\x1b]21;7=rgb:aa/bb/cc\x1b\\");
    assert!(c.palette.mask.is_set(7));
    run(&mut c, b"\x1b]21;7=\x1b\\");
    assert_eq!(c.palette.current[7], original);
    assert!(!c.palette.mask.is_set(7));
}

// stream_terminal.zig:1363 "kitty color protocol set foreground"
#[test]
fn kitty_color_protocol_set_foreground() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]21;foreground=rgb:12/34/56\x1b\\");
    assert_eq!(c.foreground.get(), Some(rgb(0x12, 0x34, 0x56)));
}

// stream_terminal.zig:1378 "kitty color protocol set background"
#[test]
fn kitty_color_protocol_set_background() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]21;background=rgb:78/9a/bc\x1b\\");
    assert_eq!(c.background.get(), Some(rgb(0x78, 0x9a, 0xbc)));
}

// stream_terminal.zig:1393 "kitty color protocol set cursor"
#[test]
fn kitty_color_protocol_set_cursor() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]21;cursor=rgb:de/f0/12\x1b\\");
    assert_eq!(c.cursor.get(), Some(rgb(0xde, 0xf0, 0x12)));
}

// stream_terminal.zig:1408 "kitty color protocol reset foreground"
#[test]
fn kitty_color_protocol_reset_foreground() {
    let mut c = Colors::default();
    run(&mut c, b"\x1b]21;foreground=rgb:11/22/33\x1b\\");
    assert!(c.foreground.get().is_some());
    run(&mut c, b"\x1b]21;foreground=\x1b\\");
    assert_eq!(c.foreground.get(), None);
}

// stream_terminal.zig:1424 "kitty color protocol query responses"
#[test]
fn kitty_color_protocol_query_responses() {
    let mut c = Colors::default();
    assert_eq!(run(&mut c, b"\x1b]21;background=?\x1b\\").0, b"\x1b]21;background=\x1b\\");
    run(&mut c, b"\x1b]21;foreground=rgb:12/34/56;2=rgb:aa/bb/cc\x1b\\");
    assert_eq!(
        run(&mut c, b"\x1b]21;foreground=?;background=?;2=?\x1b\\").0,
        b"\x1b]21;foreground=rgb:12/34/56;background=;2=rgb:aa/bb/cc\x1b\\"
    );
    // Beyond ghostty's test: keys with no terminal colour are left out, a query for only those
    // replies nothing, and the cursor does not fall back to the foreground.
    assert_eq!(run(&mut c, b"\x1b]21;visual_bell=?\x07").0, b"");
    assert_eq!(run(&mut c, b"\x1b]21;cursor=?;visual_bell=?\x07").0, b"\x1b]21;cursor=\x07");
}

// stream_terminal.zig:1464 "palette dirty flag set on color change"
#[test]
fn palette_dirty_flag_set_on_color_change() {
    let mut c = Colors::default();
    assert!(run(&mut c, b"\x1b]4;0;rgb:ff/00/00\x1b\\").1.palette);
    assert!(run(&mut c, b"\x1b]104;0\x1b\\").1.palette);
    assert!(run(&mut c, b"\x1b]21;1=rgb:00/ff/00\x1b\\").1.palette);
    // Nothing palette-related: not dirty.
    assert!(!run(&mut c, b"\x1b]10;red\x1b\\").1.palette);
}

// ---- the prefix machine (osc.zig:456 `next`, :612 `end`)

#[test]
fn prefix_machine() {
    // Empty, non-digit, and numbers ghostty has no parser for.
    for bad in [
        &b""[..],
        b"X",
        b";",
        b"3",
        b"30",
        b"300",
        b"6",
        b"55",
        b"77",
        b"552",
        b"3;x",
        b"6;x",
        b"55;x",
        b"999;x",
        b"1a;x",
        b"20;x",
        b"106;x",
        b"120;x",
    ] {
        assert_eq!(p(bad), None, "{:?}", core::str::from_utf8(bad));
    }
    // Without a `;` only the colour operations parse.
    for bare in [
        &b"0"[..],
        b"1",
        b"2",
        b"7",
        b"8",
        b"9",
        b"21",
        b"22",
        b"52",
        b"66",
        b"72",
        b"133",
        b"777",
        b"1337",
        b"3008",
        b"5522",
    ] {
        assert_eq!(p(bare), None, "{:?}", core::str::from_utf8(bare));
    }
    assert_eq!(reqs(&p(b"4").unwrap()), []);
    assert_eq!(reqs(&p(b"104").unwrap()), [Request::ResetPalette]);
    assert_eq!(reqs(&p(b"112").unwrap()), [Request::Reset(Target::Dynamic(Dynamic::Cursor))]);
}

// ghostty's machine accepts only `;` or `4` after `10`, so its OSC 105 parser is unreachable
// (vt-inventory §7 "OSC 105 … UNREACHABLE"). Fixed here: 105 resets special colours.
#[test]
fn osc_105_is_reachable_here_unlike_ghostty() {
    assert_eq!(reqs(&p(b"105").unwrap()), [Request::ResetSpecial]);
    assert_eq!(reqs(&p(b"105;2;9;x").unwrap()), [Request::Reset(Target::Special(Special::Blink))]);
    assert!(matches!(
        p(b"105;1"),
        Some(Command::ColorOperation(ColorOperation { op: Operation::ResetSpecial, .. }))
    ));
}

#[test]
fn capture_limits() {
    // Colour operations use the fixed buffer but append no NUL.
    let exact = repeat(b"4;", b';', MAX_BUF);
    assert!(p(&exact).is_some());
    assert_eq!(p(&repeat(b"4;", b';', MAX_BUF + 1)), None);
    // The allocating captures have no limit.
    for prefix in [&b"52;c;"[..], b"72;", b"5522;"] {
        let big = repeat(prefix, b'A', MAX_BUF * 3);
        assert!(p(&big).is_some(), "{:?}", core::str::from_utf8(prefix));
        assert!(unbounded(&big));
    }
    assert!(unbounded(b"66;"));
    assert!(!unbounded(b"0;"));
    assert!(!unbounded(b"5"));
    // OSC 66 text is capped at 4096 itself.
    assert!(p(&repeat(b"66;;", b'a', MAX_TEXT_SIZING_PAYLOAD)).is_some());
    assert_eq!(p(&repeat(b"66;;", b'a', MAX_TEXT_SIZING_PAYLOAD + 1)), None);
    // Every NUL-appending parser fails at exactly MAX_BUF.
    for prefix in [&b"1;"[..], b"7;", b"22;", b"9;", b"9;2;", b"1337;CurrentDir="] {
        // Capture starts after the number's `;`.
        let head = prefix.iter().position(|&b| b == b';').unwrap() + 1;
        let n = MAX_BUF - (prefix.len() - head);
        assert_eq!(p(&repeat(prefix, b'a', n)), None, "{:?}", core::str::from_utf8(prefix));
        assert!(p(&repeat(prefix, b'a', n - 1)).is_some(), "{:?}", core::str::from_utf8(prefix));
    }
    let mut link = b"8;;".to_vec();
    link.resize(MAX_BUF + 2, b'u');
    assert_eq!(p(&link), None);
    link.pop();
    assert!(p(&link).is_some());
    let mut note = b"777;notify;t;".to_vec();
    note.resize(MAX_BUF + 4, b'b');
    assert_eq!(p(&note), None);
}

// ---- vt-inventory §7, a test a row not covered above

#[test]
fn inventory_titles_and_icon() {
    assert_eq!(p(b"0;t"), Some(Command::ChangeWindowTitle(b"t")));
    assert_eq!(p(b"1;t"), Some(Command::ChangeWindowIcon(b"t")));
    assert_eq!(p(b"2;t"), Some(Command::ChangeWindowTitle(b"t")));
    // Bytes are kept raw; invalid UTF-8 is the handler's to ignore.
    assert_eq!(p(b"2;\xff"), Some(Command::ChangeWindowTitle(b"\xff")));
}

#[test]
fn inventory_osc_4_palette_set_and_query() {
    let cmd = p(b"4;1;red;256;blue;300;red;2;green").unwrap();
    // 300 is no target: parsing stops there.
    assert_eq!(
        reqs(&cmd),
        [
            Request::Set { target: Target::Palette(1), color: RED },
            Request::Set { target: Target::Special(Special::Bold), color: BLUE }
        ]
    );
    assert_eq!(reqs(&p(b"4;7;?").unwrap()), [Request::Query(Target::Palette(7))]);
    // A bad spec stops too.
    assert_eq!(reqs(&p(b"4;1;nope;2;red").unwrap()), []);
}

#[test]
fn inventory_osc_5_special() {
    assert_eq!(
        reqs(&p(b"5;4;red;5;red").unwrap()),
        [Request::Set { target: Target::Special(Special::Italic), color: RED }]
    );
    // Parsed, not applied.
    let mut c = Colors::default();
    let before = c.clone();
    let (out, _) = run(&mut c, b"\x1b]5;0;red;0;?\x1b\\");
    assert_eq!(c, before);
    assert_eq!(out, b"");
}

#[test]
fn inventory_osc_9_9_and_9_12() {
    assert_eq!(p(b"9;9;/tmp"), Some(Command::ReportPwd(b"/tmp")));
    assert_eq!(
        p(b"9;12"),
        Some(Command::SemanticPrompt(SemanticPrompt {
            action: PromptAction::FreshLineNewPrompt,
            options: b""
        }))
    );
    assert_eq!(
        p(b"9;hello"),
        Some(Command::ShowDesktopNotification { title: b"", body: b"hello" })
    );
}

#[test]
fn inventory_osc_13_to_19_parsed_ignored() {
    for code in 13..=19u8 {
        let input = std::format!("{code};red");
        let cmd = p(input.as_bytes()).unwrap();
        let d = Dynamic::from_code(code).unwrap();
        assert_eq!(reqs(&cmd), [Request::Set { target: Target::Dynamic(d), color: RED }]);
        let mut c = Colors::default();
        run_colors(&cmd, &mut c, &mut |_: &[u8]| panic!("no reply"));
        assert_eq!(c, Colors::default());
    }
    // 10 with several values runs on through 19 and stops.
    let many = p(b"10;red;red;red;red;red;red;red;red;red;red;red").unwrap();
    assert_eq!(reqs(&many).len(), 10);
}

#[test]
fn inventory_rgb_spec_forms() {
    for spec in [
        "rgb:f/0/0",
        "rgb:ff/00/00",
        "rgb:fff/000/000",
        "rgb:ffff/0000/0000",
        "rgbi:1/0/0",
        "#f00",
        "#ff0000",
        "#fff000000",
        "#ffff00000000",
        "ff0000",
        "red",
        " Red ",
    ] {
        let input = std::format!("10;{spec}");
        assert_eq!(
            reqs(&p(input.as_bytes()).unwrap()),
            [Request::Set { target: Target::Dynamic(Dynamic::Foreground), color: RED }],
            "{spec}"
        );
    }
}

#[test]
fn inventory_osc_21_applied_keys() {
    let mut c = Colors::default();
    let before = c.clone();
    run(&mut c, b"\x1b]21;selection_foreground=red;cursor_text=red;visual_bell=red;second_transparent_background=red;selection_background=red\x1b\\");
    assert_eq!(c, before);
    run(&mut c, b"\x1b]21;255=red;foreground=red;background=red;cursor=red\x1b\\");
    assert_eq!(c.palette.current[255], RED);
    assert_eq!(
        (c.foreground.get(), c.background.get(), c.cursor.get()),
        (Some(RED), Some(RED), Some(RED))
    );
}

#[test]
fn inventory_osc_22_unknown_ignored() {
    match p(b"22;not-a-shape").unwrap() {
        Command::MouseShape(name) => assert_eq!(MouseShape::from_name(name), None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn inventory_osc_52_kinds() {
    assert_eq!(Clipboard::from_kind(b'c'), Clipboard::Standard);
    assert_eq!(Clipboard::from_kind(b's'), Clipboard::Selection);
    assert_eq!(Clipboard::from_kind(b'p'), Clipboard::Primary);
    assert_eq!(Clipboard::from_kind(b'q'), Clipboard::Standard);
    assert_eq!(p(b"52;p;?"), Some(Command::ClipboardContents { kind: b'p', data: b"?" }));
    assert_eq!(p(b"52;"), None);
    assert_eq!(p(b"52;pp;x"), None);
}

#[test]
fn inventory_osc_104_resets_only_modified() {
    let mut def = DEFAULT_PALETTE;
    def[9] = rgb(1, 1, 1);
    let mut c = Colors { palette: color::DynamicPalette::new(def), ..Colors::default() };
    c.palette.current[3] = rgb(7, 7, 7); // drawn but not through OSC: not in the mask
    run(&mut c, b"\x1b]4;9;red\x1b\\");
    run(&mut c, b"\x1b]104\x1b\\");
    assert_eq!(c.palette.current[9], rgb(1, 1, 1));
    assert_eq!(c.palette.current[3], rgb(7, 7, 7));
}

#[test]
fn inventory_osc_110_with_args_is_noop() {
    let mut c = Colors { foreground: DynamicRgb::new(BLUE), ..Colors::default() };
    run(&mut c, b"\x1b]10;red\x1b\\");
    run(&mut c, b"\x1b]110;x\x1b\\");
    assert_eq!(c.foreground.get(), Some(RED));
    run(&mut c, b"\x1b]110\x1b\\");
    assert_eq!(c.foreground.get(), Some(BLUE));
}

#[test]
fn inventory_any_other_number_dropped() {
    for n in [3, 6, 11_000, 50, 99, 1000, 4000, 9999] {
        let input = std::format!("{n};x");
        let r = p(input.as_bytes());
        // 11000 is not 110 (three digits only).
        assert!(r.is_none(), "{n}: {r:?}");
    }
}

#[test]
fn reply_terminators() {
    assert_eq!(Terminator::St.bytes(), b"\x1b\\");
    assert_eq!(Terminator::Bel.bytes(), b"\x07");
    let mut c = Colors::default();
    assert_eq!(run(&mut c, b"\x1b]4;0;?\x07").0, b"\x1b]4;0;rgb:1d1d/1f1f/2121\x07");
    assert_eq!(run(&mut c, b"\x1b]4;255;?\x1b\\").0, b"\x1b]4;255;rgb:eeee/eeee/eeee\x1b\\");
    // Special colours are not answered.
    assert_eq!(run(&mut c, b"\x1b]4;256;?\x1b\\").0, b"");
}

#[test]
fn osc_7770_blaze_term_api() {
    assert!(unbounded(b"7770;"));
    assert!(!unbounded(b"7770"));
    assert_eq!(
        p(b"7770;{\"op\":\"ping\"}"),
        Some(Command::BlazeTermApi(b"{\"op\":\"ping\"}"))
    );
    assert_eq!(p(b"7770;"), None);
    assert_eq!(p(b"7770"), None);
    let big = repeat(b"7770;", b'x', MAX_BUF * 3);
    assert_eq!(p(&big), Some(Command::BlazeTermApi(&big[5..])));
}

// Property: parsing never panics, on prefixes ghostty knows followed by bytes drawn from the
// separators and characters the parsers look at; and a command's colour lists always end.
#[test]
fn parse_never_panics() {
    const PREFIXES: [&[u8]; 27] = [
        b"0;", b"1;", b"2;", b"4;", b"5;", b"7;", b"8;", b"9;", b"10;", b"12;", b"19;", b"21;",
        b"22;", b"52;", b"66;", b"72;", b"104;", b"105;", b"110;", b"133;", b"777;", b"1337;",
        b"3008;", b"5522;", b"7770;", b"9;4;", b"",
    ];
    const ALPHABET: &[u8] = b";:=?0123459abfAILNPDCrgbi#/ .,-+_\x00\x1b\x7f\xc3\xa9\xff";
    blaze_sim::check(blaze_sim::Config::cases(3000), |tape| {
        let prefix = *tape.pick(&PREFIXES, "prefix").unwrap();
        let tail = tape.vec(0..=40, "tail", |t| *t.pick(ALPHABET, "byte").unwrap());
        let mut input = prefix.to_vec();
        input.extend_from_slice(&tail);
        let term = if tape.coin("bel") { Terminator::Bel } else { Terminator::St };
        if let Some(cmd) = parse(&input, term) {
            let mut c = Colors::default();
            let mut out = vec![];
            run_colors(&cmd, &mut c, &mut |b: &[u8]| out.extend_from_slice(b));
            if !out.is_empty() {
                assert!(out.starts_with(b"\x1b]"));
                assert!(out.ends_with(term.bytes()));
            }
            if let Command::SemanticPrompt(sp) = cmd {
                let _ = sp.write_command_line(&mut |_| {});
                let _ = (sp.exit_code(), sp.cl(), sp.redraw(), sp.prompt_kind());
            }
        }
    });
}
