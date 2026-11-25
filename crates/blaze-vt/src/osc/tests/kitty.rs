//! kitty_text_sizing.zig (OSC 66), kitty_dnd_protocol.zig (OSC 72),
//! kitty_clipboard_protocol.zig (OSC 5522) and encoding.zig's tests.
//!
//! ghostty runs these with an allocator (OSC 66, 72 and 5522 capture without a size limit only
//! when it has one). blaze always captures them unbounded, so every port is the allocator case.

use super::*;
use std::vec::Vec;

fn sizing(s: &[u8]) -> TextSizing<'_> {
    match p(s) {
        Some(Command::KittyTextSizing(t)) => t,
        other => panic!("not OSC 66: {other:?}"),
    }
}

fn dnd(s: &[u8], t: Terminator) -> KittyDnd<'_> {
    match parse(s, t) {
        Some(Command::KittyDndProtocol(d)) => d,
        other => panic!("not OSC 72: {other:?}"),
    }
}

fn clip(s: &[u8]) -> KittyClipboard<'_> {
    match p(s) {
        Some(Command::KittyClipboardProtocol(c)) => c,
        other => panic!("not OSC 5522: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// OSC 66

// kitty_text_sizing.zig:148 "OSC 66: empty parameters"
#[test]
fn osc_66_empty_parameters() {
    let t = sizing(b"66;;bobr");
    assert_eq!(t.scale, 1);
    assert_eq!(t.text, b"bobr");
}

// kitty_text_sizing.zig:162 "OSC 66: single parameter"
#[test]
fn osc_66_single_parameter() {
    let t = sizing(b"66;s=2;kurwa");
    assert_eq!(t.scale, 2);
    assert_eq!(t.text, b"kurwa");
}

// kitty_text_sizing.zig:176 "OSC 66: multiple parameters"
#[test]
fn osc_66_multiple_parameters() {
    let t = sizing(b"66;s=2:w=7:n=13:d=15:v=1:h=2;long");
    assert_eq!(t.scale, 2);
    assert_eq!(t.width, 7);
    assert_eq!(t.numerator, 13);
    assert_eq!(t.denominator, 15);
    assert_eq!(t.valign, VAlign::Bottom);
    assert_eq!(t.halign, HAlign::Center);
    assert_eq!(t.text, b"long");
}

// kitty_text_sizing.zig:195 "OSC 66: scale is zero"
#[test]
fn osc_66_scale_is_zero() {
    assert_eq!(sizing(b"66;s=0;nope").scale, 1);
}

// kitty_text_sizing.zig:208 "OSC 66: invalid parameters"
#[test]
fn osc_66_invalid_parameters() {
    let t = sizing(b"66;w=8:v=3:n=16;");
    assert_eq!(t.width, 0);
    assert_eq!(t.valign, VAlign::Top);
    assert_eq!(t.numerator, 0);
}

// kitty_text_sizing.zig:222 "OSC 66: UTF-8"
#[test]
fn osc_66_utf_8() {
    let text = "👻魑魅魍魉ゴースッティ".as_bytes();
    let mut input = b"66;;".to_vec();
    input.extend_from_slice(text);
    assert_eq!(sizing(&input).text, text);
}

// kitty_text_sizing.zig:235 "OSC 66: unsafe UTF-8"
#[test]
fn osc_66_unsafe_utf_8() {
    assert_eq!(p(b"66;;\n"), None);
}

// kitty_text_sizing.zig:246 "OSC 66: overlong UTF-8"
#[test]
fn osc_66_overlong_utf_8() {
    let mut input = b"66;;".to_vec();
    for _ in 0..1025 {
        input.extend_from_slice(b"bobr");
    }
    assert_eq!(p(&input), None);
    // Exactly the limit is accepted (and beyond MAX_BUF: OSC 66 capture is unbounded).
    input.truncate(4 + MAX_TEXT_SIZING_PAYLOAD);
    assert_eq!(sizing(&input).text.len(), MAX_TEXT_SIZING_PAYLOAD);
}

// Beyond ghostty's tests: the parameter forms its parser accepts and skips.
#[test]
fn osc_66_parameter_edges() {
    // No `;` before the text: invalid.
    assert_eq!(p(b"66;s=2"), None);
    // A key must be one character with a value; bad pairs are skipped, good ones still apply.
    let t = sizing(b"66;ss=2:s:x=1:w=3;t");
    assert_eq!((t.scale, t.width), (1, 3));
    // Zig parseInt(u4): a sign is allowed, `_` between digits too, 16 overflows.
    assert_eq!(sizing(b"66;s=+3;t").scale, 3);
    assert_eq!(sizing(b"66;n=1_5;t").numerator, 15);
    assert_eq!(sizing(b"66;n=-0;t").numerator, 0);
    // A second `=` ends the value (`split` on `=`).
    assert_eq!(sizing(b"66;s=2=3;t").scale, 2);
    assert_eq!(sizing(b"66;h=1:v=2;t").halign, HAlign::Right);
    assert_eq!(sizing(b"66;h=1:v=2;t").valign, VAlign::Center);
    // C1 and DEL are not safe; a `;` in the text is.
    assert_eq!(p(b"66;;a\xc2\x85b"), None);
    assert_eq!(p(b"66;;a\x7fb"), None);
    assert_eq!(sizing(b"66;;a;b").text, b"a;b");
}

// encoding.zig:29 isSafeUtf8
#[test]
fn is_safe_utf8_cases() {
    assert!(is_safe_utf8(b"Hello world!"));
    assert!(is_safe_utf8("安全的ユニコード☀️".as_bytes()));
    assert!(!is_safe_utf8(b"No linebreaks\nallowed"));
    assert!(!is_safe_utf8(b"\x07no bells"));
    assert!(!is_safe_utf8(b"\x1b]9;no OSCs\x1b\\\x1b[m"));
    assert!(!is_safe_utf8(b"\x9f8-bit escapes are clever, but no"));
}

// ---------------------------------------------------------------------------------------------
// OSC 72

// kitty_dnd_protocol.zig:150 "OSC 72: metadata only, no payload"
#[test]
fn osc_72_metadata_only_no_payload() {
    let d = dnd(b"72;t=a", Terminator::St);
    assert_eq!(d.metadata, b"t=a");
    assert_eq!(d.payload, None);
}

// kitty_dnd_protocol.zig:165 "OSC 72: metadata and empty payload"
#[test]
fn osc_72_metadata_and_empty_payload() {
    let d = dnd(b"72;t=a;", Terminator::St);
    assert_eq!(d.metadata, b"t=a");
    assert_eq!(d.payload, Some(&b""[..]));
}

// kitty_dnd_protocol.zig:180 "OSC 72: metadata and non-empty payload"
#[test]
fn osc_72_metadata_and_non_empty_payload() {
    let d = dnd(b"72;t=a:i=5;text/plain text/uri-list", Terminator::St);
    assert_eq!(d.metadata, b"t=a:i=5");
    assert_eq!(d.payload, Some(&b"text/plain text/uri-list"[..]));
}

// kitty_dnd_protocol.zig:195 "OSC 72: readOption .t valid event types"
#[test]
fn osc_72_read_option_t_valid_event_types() {
    use DndEvent as E;
    let cases: [(&[u8], E); 13] = [
        (b"72;t=a", E::AcceptDrops),
        (b"72;t=A", E::StopAcceptingDrops),
        (b"72;t=m", E::DropMove),
        (b"72;t=M", E::DropDropped),
        (b"72;t=r", E::RequestData),
        (b"72;t=R", E::RequestError),
        (b"72;t=o", E::OfferDrag),
        (b"72;t=p", E::PresentData),
        (b"72;t=P", E::ChangeDragImage),
        (b"72;t=e", E::DragOfferEvent),
        (b"72;t=E", E::DragOfferError),
        (b"72;t=k", E::UriListData),
        (b"72;t=q", E::Query),
    ];
    for (input, want) in cases {
        assert_eq!(dnd(input, Terminator::St).event(), Some(want));
    }
}

// kitty_dnd_protocol.zig:227 "OSC 72: readOption .t unknown value returns null"
#[test]
fn osc_72_read_option_t_unknown_value_returns_null() {
    assert_eq!(dnd(b"72;t=z", Terminator::St).event(), None);
}

// kitty_dnd_protocol.zig:241 "OSC 72: readOption integer keys"
#[test]
fn osc_72_read_option_integer_keys() {
    let d = dnd(b"72;t=m:i=3:x=10:y=5:X=320:Y=200:o=1:m=0", Terminator::St);
    assert_eq!(d.int(DndInt::I), Some(3));
    assert_eq!(d.int(DndInt::X), Some(10));
    assert_eq!(d.int(DndInt::Y), Some(5));
    assert_eq!(d.int(DndInt::BigX), Some(320));
    assert_eq!(d.int(DndInt::BigY), Some(200));
    assert_eq!(d.int(DndInt::O), Some(1));
    assert_eq!(d.int(DndInt::M), Some(0));
}

// kitty_dnd_protocol.zig:261 "OSC 72: readOption negative sentinel (-1 for drag leave)"
#[test]
fn osc_72_read_option_negative_sentinel() {
    let d = dnd(b"72;t=m:x=-1:y=-1", Terminator::St);
    assert_eq!(d.int(DndInt::X), Some(-1));
    assert_eq!(d.int(DndInt::Y), Some(-1));
}

// kitty_dnd_protocol.zig:276 "OSC 72: readOption case-sensitive key matching"
#[test]
fn osc_72_read_option_case_sensitive_key_matching() {
    let d = dnd(b"72;x=10:Y=200", Terminator::St);
    assert_eq!(d.int(DndInt::X), Some(10));
    assert_eq!(d.int(DndInt::BigX), None);
    assert_eq!(d.int(DndInt::BigY), Some(200));
    assert_eq!(d.int(DndInt::Y), None);
}

// kitty_dnd_protocol.zig:294 "OSC 72: readOption absent key returns null"
#[test]
fn osc_72_read_option_absent_key_returns_null() {
    let d = dnd(b"72;t=a", Terminator::St);
    assert_eq!(d.int(DndInt::I), None);
    assert_eq!(d.int(DndInt::X), None);
    assert_eq!(d.int(DndInt::BigX), None);
    assert_eq!(d.int(DndInt::M), None);
}

// kitty_dnd_protocol.zig:311 "OSC 72: readOption malformed integer returns null"
#[test]
fn osc_72_read_option_malformed_integer_returns_null() {
    assert_eq!(dnd(b"72;x=notanumber", Terminator::St).int(DndInt::X), None);
}

// kitty_dnd_protocol.zig:325 "OSC 72: BEL terminator recorded"
#[test]
fn osc_72_bel_terminator_recorded() {
    assert_eq!(dnd(b"72;t=q", Terminator::Bel).terminator, Terminator::Bel);
    assert_eq!(dnd(b"72;t=q", Terminator::St).terminator, Terminator::St);
}

// Beyond ghostty's tests: the shared option reader's whitespace and prefix rules.
#[test]
fn osc_72_option_reader_edges() {
    // Whitespace around keys and values is skipped (Zig's set includes VT and FF).
    let d = dnd(b"72; x = 7 :\x0bY\x0c=\t8", Terminator::St);
    assert_eq!(d.int(DndInt::X), Some(7));
    assert_eq!(d.int(DndInt::BigY), Some(8));
    // A key that only starts with the name ends the search (it does not skip ahead).
    assert_eq!(dnd(b"72;xx=1:x=2", Terminator::St).int(DndInt::X), None);
    // Out of i32 range is null; the unbounded capture takes metadata past MAX_BUF.
    assert_eq!(dnd(b"72;x=2147483648", Terminator::St).int(DndInt::X), None);
    let mut long = b"72;".to_vec();
    long.extend(core::iter::repeat_n(b'a', MAX_BUF + 10));
    assert_eq!(dnd(&long, Terminator::St).metadata.len(), MAX_BUF + 10);
    assert!(unbounded(b"72;"));
}

// ---------------------------------------------------------------------------------------------
// OSC 5522

/// Every option of one command, for whole-value comparison.
#[derive(Debug, PartialEq, Default)]
struct Opts<'a> {
    id: Option<&'a [u8]>,
    loc: Option<ClipboardLocation>,
    mime: Option<&'a [u8]>,
    name: Option<&'a [u8]>,
    password: Option<&'a [u8]>,
    pw: Option<&'a [u8]>,
    status: Option<ClipboardStatus>,
    ty: Option<ClipboardOperation>,
}

fn opts<'a>(c: &KittyClipboard<'a>) -> Opts<'a> {
    Opts {
        id: c.id(),
        loc: c.loc(),
        mime: c.text(ClipboardText::Mime),
        name: c.text(ClipboardText::Name),
        password: c.text(ClipboardText::Password),
        pw: c.text(ClipboardText::Pw),
        status: c.status(),
        ty: c.operation(),
    }
}

use ClipboardOperation as Op;
use ClipboardStatus as St;

// kitty_clipboard_protocol.zig:178 "OSC: 5522: empty metadata and missing payload"
#[test]
fn osc_5522_empty_metadata_and_missing_payload() {
    let c = clip(b"5522;");
    assert_eq!(c.metadata, b"");
    assert_eq!(c.payload, None);
    assert_eq!(opts(&c), Opts::default());
}

// kitty_clipboard_protocol.zig:201 "OSC: 5522: empty metadata and empty payload"
#[test]
fn osc_5522_empty_metadata_and_empty_payload() {
    let c = clip(b"5522;;");
    assert_eq!(c.metadata, b"");
    assert_eq!(c.payload, Some(&b""[..]));
    assert_eq!(opts(&c), Opts::default());
}

// kitty_clipboard_protocol.zig:224 "OSC: 5522: non-empty metadata and payload"
#[test]
fn osc_5522_non_empty_metadata_and_payload() {
    let c = clip(b"5522;type=read;dGV4dC9wbGFpbg==");
    assert_eq!(c.metadata, b"type=read");
    assert_eq!(c.payload, Some(&b"dGV4dC9wbGFpbg=="[..]));
    assert_eq!(opts(&c), Opts { ty: Some(Op::Read), ..Opts::default() });
}

// kitty_clipboard_protocol.zig:247 "OSC: 5522: empty id"
#[test]
fn osc_5522_empty_id() {
    assert_eq!(clip(b"5522;id=").id(), None);
}

// kitty_clipboard_protocol.zig:261 "OSC: 5522: valid id"
#[test]
fn osc_5522_valid_id() {
    assert_eq!(
        clip(b"5522;id=5c076ad9-d36f-4705-847b-d4dbf356cc0d").id(),
        Some(&b"5c076ad9-d36f-4705-847b-d4dbf356cc0d"[..])
    );
}

// kitty_clipboard_protocol.zig:275 "OSC: 5522: invalid id"
#[test]
fn osc_5522_invalid_id() {
    assert_eq!(clip(b"5522;id=*42*").id(), None);
}

// kitty_clipboard_protocol.zig:289 "OSC: 5522: invalid status"
#[test]
fn osc_5522_invalid_status() {
    assert_eq!(clip(b"5522;status=BOBR").status(), None);
}

// kitty_clipboard_protocol.zig:303 "OSC: 5522: valid status"
#[test]
fn osc_5522_valid_status() {
    assert_eq!(clip(b"5522;status=DONE").status(), Some(St::Done));
}

// kitty_clipboard_protocol.zig:317 "OSC: 5522: invalid location"
#[test]
fn osc_5522_invalid_location() {
    assert_eq!(clip(b"5522;loc=bobr").loc(), None);
}

// kitty_clipboard_protocol.zig:331 "OSC: 5522: valid location"
#[test]
fn osc_5522_valid_location() {
    assert_eq!(clip(b"5522;loc=primary").loc(), Some(ClipboardLocation::Primary));
}

// kitty_clipboard_protocol.zig:345 "OSC: 5522: password 1"
#[test]
fn osc_5522_password_1() {
    let c = clip(b"5522;pw=R2hvc3R0eQ==:name=Qk9CUiBLVVJXQQ==");
    assert_eq!(c.text(ClipboardText::Pw), Some(&b"R2hvc3R0eQ=="[..]));
    assert_eq!(c.text(ClipboardText::Name), Some(&b"Qk9CUiBLVVJXQQ=="[..]));
}

// kitty_clipboard_protocol.zig:360 "OSC: 5522: password 2"
#[test]
fn osc_5522_password_2() {
    assert_eq!(
        clip(b"5522;password=R2hvc3R0eQ==").text(ClipboardText::Password),
        Some(&b"R2hvc3R0eQ=="[..])
    );
}

/// The examples: input, payload, and the options set.
fn example(input: &[u8], payload: Option<&[u8]>, want: Opts<'_>) {
    let c = clip(input);
    assert_eq!(c.payload, payload, "{}", std::string::String::from_utf8_lossy(input));
    assert_eq!(opts(&c), want, "{}", std::string::String::from_utf8_lossy(input));
}

const MIME: &[u8] = b"dGV4dC9wbGFpbg==";

// kitty_clipboard_protocol.zig:374 "OSC: 5522: example 1"
#[test]
fn osc_5522_example_1() {
    example(
        b"5522;type=read:status=OK",
        None,
        Opts { status: Some(St::Ok), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:396 "OSC: 5522: example 2"
#[test]
fn osc_5522_example_2() {
    example(
        b"5522;type=read:mime=dGV4dC9wbGFpbg==;R2hvc3R0eQ==",
        Some(b"R2hvc3R0eQ=="),
        Opts { mime: Some(MIME), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:418 "OSC: 5522: example 3"
#[test]
fn osc_5522_example_3() {
    example(
        b"5522;type=read:status=OK",
        None,
        Opts { status: Some(St::Ok), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:440 "OSC: 5522: example 4"
#[test]
fn osc_5522_example_4() {
    example(b"5522;type=write", None, Opts { ty: Some(Op::Write), ..Opts::default() });
}

// kitty_clipboard_protocol.zig:462 "OSC: 5522: example 5"
#[test]
fn osc_5522_example_5() {
    example(
        b"5522;type=wdata:mime=dGV4dC9wbGFpbg==;R2hvc3R0eQ==",
        Some(b"R2hvc3R0eQ=="),
        Opts { mime: Some(MIME), ty: Some(Op::Wdata), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:484 "OSC: 5522: example 6"
#[test]
fn osc_5522_example_6() {
    example(b"5522;type=wdata", None, Opts { ty: Some(Op::Wdata), ..Opts::default() });
}

// kitty_clipboard_protocol.zig:506 "OSC: 5522: example 7"
#[test]
fn osc_5522_example_7() {
    example(
        b"5522;type=write:status=DONE",
        None,
        Opts { status: Some(St::Done), ty: Some(Op::Write), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:528 "OSC: 5522: example 8"
#[test]
fn osc_5522_example_8() {
    example(
        b"5522;type=write:status=EPERM",
        None,
        Opts { status: Some(St::Eperm), ty: Some(Op::Write), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:550 "OSC: 5522: example 9"
#[test]
fn osc_5522_example_9() {
    example(
        b"5522;type=walias:mime=dGV4dC9wbGFpbg==;dGV4dC9odG1sIGFwcGxpY2F0aW9uL2pzb24=",
        Some(b"dGV4dC9odG1sIGFwcGxpY2F0aW9uL2pzb24="),
        Opts { mime: Some(MIME), ty: Some(Op::Walias), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:572 "OSC: 5522: example 10"
#[test]
fn osc_5522_example_10() {
    example(
        b"5522;type=read:status=OK:password=Qk9CUiBLVVJXQQ==",
        None,
        Opts {
            password: Some(b"Qk9CUiBLVVJXQQ=="),
            status: Some(St::Ok),
            ty: Some(Op::Read),
            ..Opts::default()
        },
    );
}

// kitty_clipboard_protocol.zig:594 "OSC: 5522: example 11"
#[test]
fn osc_5522_example_11() {
    example(
        b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==",
        None,
        Opts { mime: Some(MIME), status: Some(St::Data), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:616 "OSC: 5522: example 12"
#[test]
fn osc_5522_example_12() {
    example(
        b"5522;type=read:mime=dGV4dC9wbGFpbg==:password=Qk9CUiBLVVJXQQ==",
        None,
        Opts {
            mime: Some(MIME),
            password: Some(b"Qk9CUiBLVVJXQQ=="),
            ty: Some(Op::Read),
            ..Opts::default()
        },
    );
}

// kitty_clipboard_protocol.zig:638 "OSC: 5522: example 13"
#[test]
fn osc_5522_example_13() {
    example(
        b"5522;type=read:status=OK",
        None,
        Opts { status: Some(St::Ok), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:660 "OSC: 5522: example 14"
#[test]
fn osc_5522_example_14() {
    example(
        b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==;Qk9CUiBLVVJXQQ==",
        Some(b"Qk9CUiBLVVJXQQ=="),
        Opts { mime: Some(MIME), status: Some(St::Data), ty: Some(Op::Read), ..Opts::default() },
    );
}

// kitty_clipboard_protocol.zig:682 "OSC: 5522: example 15"
#[test]
fn osc_5522_example_15() {
    example(
        b"5522;type=read:status=OK",
        None,
        Opts { status: Some(St::Ok), ty: Some(Op::Read), ..Opts::default() },
    );
}

// Beyond ghostty's tests: every status and type name, the identifier alphabet, the terminator,
// and the unbounded capture.
#[test]
fn osc_5522_enum_coverage_and_capture() {
    for (s, want) in [
        (&b"DATA"[..], St::Data),
        (b"DONE", St::Done),
        (b"EBUSY", St::Ebusy),
        (b"EINVAL", St::Einval),
        (b"EIO", St::Eio),
        (b"ENOSYS", St::Enosys),
        (b"EPERM", St::Eperm),
        (b"OK", St::Ok),
    ] {
        let mut input = b"5522;status=".to_vec();
        input.extend_from_slice(s);
        assert_eq!(clip(&input).status(), Some(want));
    }
    for (s, want) in [
        (&b"read"[..], Op::Read),
        (b"walias", Op::Walias),
        (b"wdata", Op::Wdata),
        (b"write", Op::Write),
    ] {
        let mut input = b"5522;type=".to_vec();
        input.extend_from_slice(s);
        assert_eq!(clip(&input).operation(), Some(want));
    }
    assert_eq!(clip(b"5522;status=ok").status(), None);
    assert_eq!(clip(b"5522;id=aZ09-_+.").id(), Some(&b"aZ09-_+."[..]));
    assert_eq!(clip(b"5522;id=a b").id(), None);
    match parse(b"5522;type=read", Terminator::Bel) {
        Some(Command::KittyClipboardProtocol(c)) => assert_eq!(c.terminator, Terminator::Bel),
        other => panic!("{other:?}"),
    }
    let mut long: Vec<u8> = b"5522;type=write;".to_vec();
    long.extend(core::iter::repeat_n(b'A', MAX_BUF * 2));
    assert_eq!(clip(&long).payload.map(<[u8]>::len), Some(MAX_BUF * 2));
    assert!(unbounded(b"5522;"));
}
