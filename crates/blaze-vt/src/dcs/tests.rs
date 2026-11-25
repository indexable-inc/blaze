//! DCS: ghostty's dcs.zig and tmux/control.zig tests by name, one test per vt-inventory.md §8
//! row, the parser driving the handler end to end, and properties.

use std::vec::Vec as StdVec;

use blaze_own::{Erased, Inline, testing};

use super::tmux::{ControlParser, Notification, PaneId, SessionId, WindowId};
use super::{DcsCommand, DcsHandler, DcsOpts, DcsStats, Decrqss, Refusal};
use crate::parser::{Csi, Dcs, Esc, Parser, Sink, Terminator};

/// The parser's notifications: borrowed bytes, so byte-string literals coerce.
type Note<'a> = Notification<&'a [u8]>;

const OPTS: DcsOpts = DcsOpts { tmux_control_mode: true, max_bytes: DcsOpts::MAX_BYTES };

fn handler() -> DcsHandler<'static> {
    DcsHandler::new_in(OPTS, testing::heap().alloc())
}

fn hook<'h>(
    h: &'h mut DcsHandler<'_>,
    intermediates: &[u8],
    params: &[u16],
    final_byte: u8,
) -> Option<DcsCommand<'h>> {
    h.hook(&Dcs { intermediates, params, final_byte })
}

fn put_all(h: &mut DcsHandler<'_>, bytes: &[u8]) {
    for &b in bytes {
        assert_eq!(h.put(b), None, "no command mid-string");
    }
}

/// The keys of an XTGETTCAP command, owned.
fn keys(cmd: Option<DcsCommand<'_>>) -> StdVec<StdVec<u8>> {
    match cmd {
        Some(DcsCommand::XtGetTcap(x)) => x.keys().map(<[u8]>::to_vec).collect(),
        other => panic!("not XTGETTCAP: {other:?}"),
    }
}

// --- dcs.zig ---

#[test]
fn unknown_dcs_command() {
    // dcs.zig:298 "unknown DCS command"
    let mut h = handler();
    assert_eq!(hook(&mut h, b"", &[], b'A'), None);
    assert!(!h.is_active(), "ignored");
    assert_eq!(h.unhook(), None);
}

#[test]
fn xtgettcap_command() {
    // dcs.zig:310 "XTGETTCAP command"
    let mut h = handler();
    assert_eq!(hook(&mut h, b"+", &[], b'q'), None);
    put_all(&mut h, b"536D756C78");
    assert_eq!(keys(h.unhook()), [b"536D756C78".to_vec()]);
}

#[test]
fn xtgettcap_mixed_case() {
    // dcs.zig:325 "XTGETTCAP mixed case"
    let mut h = handler();
    assert_eq!(hook(&mut h, b"+", &[], b'q'), None);
    put_all(&mut h, b"536d756C78");
    assert_eq!(keys(h.unhook()), [b"536D756C78".to_vec()]);
}

#[test]
fn xtgettcap_command_multiple_keys() {
    // dcs.zig:340 "XTGETTCAP command multiple keys"
    let mut h = handler();
    hook(&mut h, b"+", &[], b'q');
    put_all(&mut h, b"536D756C78;536D756C78");
    assert_eq!(keys(h.unhook()), [b"536D756C78".to_vec(), b"536D756C78".to_vec()]);
}

#[test]
fn xtgettcap_command_invalid_data() {
    // dcs.zig:356 "XTGETTCAP command invalid data"
    let mut h = handler();
    hook(&mut h, b"+", &[], b'q');
    put_all(&mut h, b"who;536D756C78");
    assert_eq!(keys(h.unhook()), [b"WHO".to_vec(), b"536D756C78".to_vec()]);
}

#[test]
fn decrqss_command() {
    // dcs.zig:372 "DECRQSS command"
    let mut h = handler();
    assert_eq!(hook(&mut h, b"$", &[], b'q'), None);
    put_all(&mut h, b"m");
    assert_eq!(h.unhook(), Some(DcsCommand::Decrqss(Decrqss::Sgr)));
}

#[test]
fn decrqss_invalid_command() {
    // dcs.zig:386 "DECRQSS invalid command"
    let mut h = handler();
    hook(&mut h, b"$", &[], b'q');
    put_all(&mut h, b"z");
    assert_eq!(h.unhook(), Some(DcsCommand::Decrqss(Decrqss::None)));

    hook(&mut h, b"$", &[], b'q');
    put_all(&mut h, b"\" q");
    assert_eq!(h.unhook(), None, "three payload bytes: no reply at all");
}

#[test]
fn tmux_enter_and_implicit_exit() {
    // dcs.zig:408 "tmux enter and implicit exit"
    let mut h = handler();
    assert_eq!(hook(&mut h, b"", &[1000], b'p'), Some(DcsCommand::Tmux(Note::Enter)));
    assert_eq!(h.unhook(), Some(DcsCommand::Tmux(Note::Exit)));
}

// --- vt-inventory.md §8, a test per row ---

#[test]
fn inventory_8_tmux_control_mode_enter_exit_and_build_option() {
    // Row "DCS 1000 p": enter with exactly one parameter 1000 and no intermediates; off when the
    // build option is (dcs.zig:55).
    let mut h = handler();
    for (inter, params) in
        [(&b""[..], &[][..]), (b"", &[1000, 1][..]), (b"", &[999][..]), (b"$", &[1000][..])]
    {
        assert_eq!(hook(&mut h, inter, params, b'p'), None, "{inter:?} {params:?}");
        assert_eq!(h.unhook(), None);
    }
    let mut off =
        DcsHandler::new_in(DcsOpts { tmux_control_mode: false, ..OPTS }, testing::heap().alloc());
    assert_eq!(hook(&mut off, b"", &[1000], b'p'), None);
    put_all(&mut off, b"%sessions-changed\n");
    assert_eq!(off.unhook(), None);
}

#[test]
fn inventory_8_tmux_notifications_come_mid_string() {
    let mut h = handler();
    hook(&mut h, b"", &[1000], b'p');
    put_all(&mut h, b"%window-add @3");
    assert_eq!(h.put(b'\n'), Some(DcsCommand::Tmux(Note::WindowAdd { window: WindowId(3) })));
    put_all(&mut h, b"%begin 1 2 0\nok\n%end 1 2 0");
    assert_eq!(h.put(b'\n'), Some(DcsCommand::Tmux(Note::BlockEnd(b"ok"))));
    assert_eq!(h.unhook(), Some(DcsCommand::Tmux(Note::Exit)));
}

#[test]
fn inventory_8_tmux_stray_line_exits_then_unhook_exits_again() {
    // control.zig:84: a line not starting with `%` breaks the session with an exit; the DCS
    // stays hooked, so its end sends another exit (dcs.zig:163).
    let mut h = handler();
    hook(&mut h, b"", &[1000], b'p');
    assert_eq!(h.put(b'x'), Some(DcsCommand::Tmux(Note::Exit)));
    put_all(&mut h, b"%sessions-changed\n");
    assert_eq!(h.unhook(), Some(DcsCommand::Tmux(Note::Exit)));
}

#[test]
fn inventory_8_tmux_over_cap_drops_without_exit() {
    // control.zig:71 and dcs.zig:115: past the cap the parser is broken and the DCS ignored, so
    // unhook sends nothing.
    let mut h = DcsHandler::new_in(DcsOpts { max_bytes: 8, ..OPTS }, testing::heap().alloc());
    hook(&mut h, b"", &[1000], b'p');
    put_all(&mut h, b"%output %1 0123456789");
    assert_eq!(h.unhook(), None);
    assert_eq!(h.stats(), DcsStats { over_cap: 1, alloc_refused: 0 });
}

#[test]
fn inventory_8_xtgettcap_upper_cased_split_and_capped() {
    // Row "DCS + q": upper-cased, split on `;`, at most 1 MB (dcs.zig:132).
    let mut h = DcsHandler::new_in(DcsOpts { max_bytes: 4, ..OPTS }, testing::heap().alloc());
    hook(&mut h, b"+", &[], b'q');
    put_all(&mut h, b"6b62");
    assert_eq!(keys(h.unhook()), [b"6B62".to_vec()], "four bytes fit");
    hook(&mut h, b"+", &[], b'q');
    put_all(&mut h, b"6b627");
    assert_eq!(h.unhook(), None, "the fifth byte drops the request");
    assert_eq!(h.stats().over_cap, 1);
}

#[test]
fn inventory_8_xtgettcap_keys_follow_ghostty_next() {
    // dcs.zig:225 `next`: an empty payload has no key, a trailing `;` adds none, an inner empty
    // key is kept.
    let mut h = handler();
    for (payload, want) in [
        (&b""[..], &[][..]),
        (b";", &[&b""[..]][..]),
        (b"AB;", &[&b"AB"[..]][..]),
        (b"AB;;CD", &[&b"AB"[..], b"", b"CD"][..]),
        (b";AB", &[&b""[..], b"AB"][..]),
    ] {
        hook(&mut h, b"+", &[], b'q');
        put_all(&mut h, payload);
        let got = keys(h.unhook());
        let want: StdVec<StdVec<u8>> = want.iter().map(|k| k.to_vec()).collect();
        assert_eq!(got, want, "{payload:?}");
    }
}

#[test]
fn inventory_8_decrqss_each_setting() {
    // Rows "DCS $ q m / r / s / SP q / other".
    let mut h = handler();
    for (payload, want) in [
        (&b"m"[..], Decrqss::Sgr),
        (b"r", Decrqss::Decstbm),
        (b"s", Decrqss::Decslrm),
        (b" q", Decrqss::Decscusr),
        (b"\"q", Decrqss::None),
        (b"\"p", Decrqss::None),
        (b"t", Decrqss::None),
        (b"", Decrqss::None),
        (b"q ", Decrqss::None),
    ] {
        hook(&mut h, b"$", &[], b'q');
        put_all(&mut h, payload);
        assert_eq!(h.unhook(), Some(DcsCommand::Decrqss(want)), "{payload:?}");
    }
}

#[test]
fn inventory_8_anything_else_is_ignored() {
    // Row "Anything else (sixel, DECUDK, ReGIS, …)": `DCS q` sixel, `DCS 1;1|` DECUDK, `DCS p`
    // ReGIS, `DCS + p` XTSETTCAP, `DCS $ t` DECRSPS.
    let mut h = handler();
    for (inter, params, f) in [
        (&b""[..], &[][..], b'q'),
        (b"", &[1, 1][..], b'|'),
        (b"", &[0][..], b'p'),
        (b"+", &[][..], b'p'),
        (b"$", &[][..], b't'),
    ] {
        assert_eq!(hook(&mut h, inter, params, f), None);
        put_all(&mut h, b"#0;2;0;0;0~~");
        assert_eq!(h.unhook(), None, "{inter:?} {params:?} {}", f as char);
    }
}

#[test]
fn an_allocation_refusal_drops_the_request_and_is_counted() {
    // A buffer the allocator will not grow: dropped as ghostty drops it (dcs.zig:115).
    let inline = Inline::<16>::new();
    let erased = Erased::new(&inline);
    let mut h = DcsHandler::new_in(OPTS, erased.handle());
    hook(&mut h, b"+", &[], b'q');
    put_all(&mut h, &[b'A'; 64]);
    assert_eq!(h.unhook(), None);
    assert_eq!(h.stats().alloc_refused, 1);
}

// --- end to end through the parser ---

/// A sink that hands DCS to the handler and keeps each command's debug text.
struct ToHandler<'a> {
    h: DcsHandler<'a>,
    seen: StdVec<std::string::String>,
}

impl Sink for ToHandler<'_> {
    fn print(&mut self, _: char) {}
    fn execute(&mut self, _: u8) {}
    fn esc(&mut self, _: &Esc<'_>) {}
    fn csi(&mut self, _: &Csi<'_>) {}
    fn osc_start(&mut self) {}
    fn osc_put(&mut self, _: u8) {}
    fn osc_end(&mut self, _: Terminator) {}
    fn dcs_hook(&mut self, dcs: &Dcs<'_>) {
        let c = self.h.hook(dcs).map(|c| std::format!("{c:?}"));
        self.seen.extend(c);
    }
    fn dcs_put(&mut self, b: u8) {
        let c = self.h.put(b).map(|c| std::format!("{c:?}"));
        self.seen.extend(c);
    }
    fn dcs_unhook(&mut self) {
        let c = self.h.unhook().map(|c| std::format!("{c:?}"));
        self.seen.extend(c);
    }
    fn apc_start(&mut self) {}
    fn apc_put(&mut self, _: &[u8]) {}
    fn apc_end(&mut self) {}
}

fn through_parser(bytes: &[u8]) -> StdVec<std::string::String> {
    let mut s = ToHandler { h: handler(), seen: StdVec::new() };
    Parser::new().feed(bytes, &mut s);
    s.seen
}

#[test]
fn the_parser_drives_the_handler() {
    assert_eq!(through_parser(b"\x1bP$qm\x1b\\"), ["Decrqss(Sgr)"]);
    assert_eq!(
        through_parser(b"\x1bP+q544e;436f\x1b\\"),
        ["XtGetTcap(XtGetTcap { data: [53, 52, 52, 69, 59, 52, 51, 54, 70] })"]
    );
    assert_eq!(
        through_parser(b"\x1bP1000p%window-add @1\n%output %2 hi\n\x1b\\"),
        [
            "Tmux(Enter)",
            "Tmux(WindowAdd { window: WindowId(1) })",
            "Tmux(Output { pane: PaneId(2), data: [104, 105] })",
            "Tmux(Exit)"
        ]
    );
    // DCS ignored by ghostty leaves nothing.
    assert!(through_parser(b"\x1bPq#0;2;0;0;0~-\x1b\\").is_empty());
}

// --- tmux/control.zig ---

fn parser() -> ControlParser<'static> {
    ControlParser::new_in(DcsOpts::MAX_BYTES, testing::heap().alloc())
}

fn feed_none(c: &mut ControlParser<'_>, bytes: &[u8]) {
    for &b in bytes {
        assert_eq!(c.put(b), Ok(None), "byte {:?}", b as char);
    }
}

#[test]
fn tmux_begin_end_empty() {
    // control.zig:599 "tmux begin/end empty"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1578922740 269 1\n");
    feed_none(&mut c, b"%end 1578922740 269 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b""))));
}

#[test]
fn tmux_begin_error_empty() {
    // control.zig:612 "tmux begin/error empty"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1578922740 269 1\n");
    feed_none(&mut c, b"%error 1578922740 269 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockErr(b""))));
}

#[test]
fn tmux_begin_end_data() {
    // control.zig:625 "tmux begin/end data"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1578922740 269 1\n");
    feed_none(&mut c, b"hello\nworld\n");
    feed_none(&mut c, b"%end 1578922740 269 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b"hello\nworld"))));
}

#[test]
fn tmux_block_payload_may_start_with_end() {
    // control.zig:639 "tmux block payload may start with %end"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1 1 1\n%end not really\nhello\n%end 1 1 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b"%end not really\nhello"))));
}

#[test]
fn tmux_block_payload_may_start_with_error() {
    // control.zig:654 "tmux block payload may start with %error"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1 1 1\n%error not really\nhello\n%end 1 1 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b"%error not really\nhello"))));
}

#[test]
fn tmux_block_may_terminate_with_real_error_after_misleading_payload() {
    // control.zig:669 "tmux block may terminate with real %error after misleading payload"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1 1 1\n%error not really\nhello\n%error 1 1 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockErr(b"%error not really\nhello"))));
}

#[test]
fn tmux_block_terminator_requires_exact_token_count() {
    // control.zig:684 "tmux block terminator requires exact token count"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1 1 1\n%end 1 1 1 trailing\nhello\n%end 1 1 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b"%end 1 1 1 trailing\nhello"))));
}

#[test]
fn tmux_block_terminator_requires_numeric_metadata() {
    // control.zig:699 "tmux block terminator requires numeric metadata"
    let mut c = parser();
    feed_none(&mut c, b"%begin 1 1 1\n%end foo bar baz\nhello\n%end 1 1 1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::BlockEnd(b"%end foo bar baz\nhello"))));
}

#[test]
fn tmux_output() {
    // control.zig:714 "tmux output"
    let mut c = parser();
    feed_none(&mut c, b"%output %42 foo bar baz");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::Output { pane: PaneId(42), data: b"foo bar baz" })));
}

#[test]
fn tmux_session_changed() {
    // control.zig:727 "tmux session-changed"
    let mut c = parser();
    feed_none(&mut c, b"%session-changed $42 foo");
    assert_eq!(
        c.put(b'\n'),
        Ok(Some(Note::SessionChanged { session: SessionId(42), name: b"foo" }))
    );
}

#[test]
fn tmux_sessions_changed() {
    // control.zig:740 "tmux sessions-changed"
    let mut c = parser();
    feed_none(&mut c, b"%sessions-changed");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::SessionsChanged)));
}

#[test]
fn tmux_sessions_changed_carriage_return() {
    // control.zig:751 "tmux sessions-changed carriage return"
    let mut c = parser();
    feed_none(&mut c, b"%sessions-changed\r");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::SessionsChanged)));
}

#[test]
fn tmux_layout_change() {
    // control.zig:762 "tmux layout-change"
    let mut c = parser();
    feed_none(
        &mut c,
        b"%layout-change @2 1234x791,0,0{617x791,0,0,0,617x791,618,0,1} 1234x791,0,0{617x791,0,0,0,617x791,618,0,1} *-",
    );
    assert_eq!(
        c.put(b'\n'),
        Ok(Some(Note::LayoutChange {
            window: WindowId(2),
            layout: b"1234x791,0,0{617x791,0,0,0,617x791,618,0,1}",
            visible_layout: b"1234x791,0,0{617x791,0,0,0,617x791,618,0,1}",
            raw_flags: b"*-",
        }))
    );
}

#[test]
fn tmux_window_add() {
    // control.zig:777 "tmux window-add"
    let mut c = parser();
    feed_none(&mut c, b"%window-add @14");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::WindowAdd { window: WindowId(14) })));
}

#[test]
fn tmux_window_renamed() {
    // control.zig:789 "tmux window-renamed"
    let mut c = parser();
    feed_none(&mut c, b"%window-renamed @42 bar");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::WindowRenamed { window: WindowId(42), name: b"bar" })));
}

#[test]
fn tmux_window_pane_changed() {
    // control.zig:802 "tmux window-pane-changed"
    let mut c = parser();
    feed_none(&mut c, b"%window-pane-changed @42 %2");
    assert_eq!(
        c.put(b'\n'),
        Ok(Some(Note::WindowPaneChanged { window: WindowId(42), pane: PaneId(2) }))
    );
}

#[test]
fn tmux_client_detached() {
    // control.zig:815 "tmux client-detached"
    let mut c = parser();
    feed_none(&mut c, b"%client-detached /dev/pts/1");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::ClientDetached { client: b"/dev/pts/1" })));
}

#[test]
fn tmux_client_session_changed() {
    // control.zig:827 "tmux client-session-changed"
    let mut c = parser();
    feed_none(&mut c, b"%client-session-changed /dev/pts/1 $2 mysession");
    assert_eq!(
        c.put(b'\n'),
        Ok(Some(Note::ClientSessionChanged {
            client: b"/dev/pts/1",
            session: SessionId(2),
            name: b"mysession"
        }))
    );
}

// --- tmux: the regexes' greedy splits and the edges ghostty's code takes ---

fn line(bytes: &[u8]) -> Option<StdVec<u8>> {
    let mut c = parser();
    feed_none(&mut c, bytes);
    c.put(b'\n').expect("no refusal").map(|n| std::format!("{n:?}").into_bytes())
}

#[test]
fn tmux_greedy_groups_split_where_the_regex_splits() {
    // `(.+) (.+) (.*)$`: group 2 longest, group 4 after the last space (possibly empty).
    let n = |l: &[u8], v: &[u8], f: &[u8]| {
        std::format!(
            "{:?}",
            Note::LayoutChange { window: WindowId(1), layout: l, visible_layout: v, raw_flags: f }
        )
        .into_bytes()
    };
    assert_eq!(line(b"%layout-change @1 a b c d"), Some(n(b"a b", b"c", b"d")));
    assert_eq!(line(b"%layout-change @1 a b "), Some(n(b"a", b"b", b"")));
    assert_eq!(line(b"%layout-change @1 a b  "), Some(n(b"a", b"b ", b"")));
    assert_eq!(line(b"%layout-change @1 a b"), None, "needs three groups");
    // `(.+) \$([0-9]+) (.+)$`: group 1 longest.
    assert_eq!(
        line(b"%client-session-changed a $1 b $2 c"),
        Some(
            std::format!(
                "{:?}",
                Note::ClientSessionChanged { client: b"a $1 b", session: SessionId(2), name: b"c" }
            )
            .into_bytes()
        )
    );
    assert_eq!(
        line(b"%client-session-changed a $1 b $2x c"),
        Some(
            std::format!(
                "{:?}",
                Note::ClientSessionChanged {
                    client: b"a",
                    session: SessionId(1),
                    name: b"b $2x c"
                }
            )
            .into_bytes()
        )
    );
}

#[test]
fn tmux_unmatched_and_unknown_lines_give_nothing_and_stay_idle() {
    for l in [
        &b"%output %x data"[..],
        b"%output %1 ",
        b"%output",
        b"%window-add @1 extra",
        b"%window-add @",
        b"%sessions-changed now",
        b"%client-detached",
        b"%window-pane-changed @1 %",
        b"%unknown thing",
        b"%output %99999999999999999999 overflow",
    ] {
        let mut c = parser();
        feed_none(&mut c, l);
        assert_eq!(c.put(b'\n'), Ok(None), "{}", std::string::String::from_utf8_lossy(l));
        assert!(!c.is_broken());
        feed_none(&mut c, b"%sessions-changed");
        assert_eq!(c.put(b'\n'), Ok(Some(Note::SessionsChanged)), "still parses after");
    }
}

#[test]
fn tmux_block_terminator_numbers_as_zig_parses_them() {
    // Zig's parseInt(usize): a sign, `_` between digits; runs of spaces collapse.
    for (term, ends) in [
        (&b"%end +1 -0 1_000"[..], true),
        (b"  %end 1  2   3", true),
        (b"%end 1 2 3\r", true),
        (b"%end -1 2 3", false),
        (b"%end _1 2 3", false),
        (b"%end 1_ 2 3", false),
        (b"%end 1 2 99999999999999999999", false),
        (b"%end 1 2", false),
    ] {
        let mut c = parser();
        feed_none(&mut c, b"%begin 1 2 3\n");
        feed_none(&mut c, term);
        let got = c.put(b'\n').expect("no refusal");
        assert_eq!(got.is_some(), ends, "{}", std::string::String::from_utf8_lossy(term));
    }
}

#[test]
fn tmux_cap_breaks_once_then_drops() {
    let mut c = ControlParser::new_in(4, testing::heap().alloc());
    feed_none(&mut c, b"%abc");
    assert_eq!(c.put(b'd'), Err(Refusal::OverCap { max_bytes: 4 }));
    assert!(c.is_broken());
    feed_none(&mut c, b"\n%sessions-changed\n");
}

#[test]
fn tmux_cap_checks_the_newline_too() {
    // The cap is checked before every byte, the ending newline included (control.zig:71): a
    // line of `max_bytes` bytes is refused at its newline; one byte shorter is answered.
    let mut c = ControlParser::new_in(14, testing::heap().alloc());
    feed_none(&mut c, b"%output %1 abc");
    assert_eq!(c.put(b'\n'), Err(Refusal::OverCap { max_bytes: 14 }));
    let mut c = ControlParser::new_in(15, testing::heap().alloc());
    feed_none(&mut c, b"%output %1 abc");
    assert_eq!(c.put(b'\n'), Ok(Some(Note::Output { pane: PaneId(1), data: b"abc" })));
    assert_eq!(c.put(b'%'), Ok(None), "the next line starts");
}

// --- properties ---

#[test]
fn any_dcs_bytes_hold_the_cap_and_never_panic() {
    const ALPHABET: &[u8] = b"%\n\r $@+;0123456789abcdefABCDEF begin end error output window-add";
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let max = tape.int_in(1..=64usize, "max_bytes");
        let mut h = DcsHandler::new_in(
            DcsOpts { tmux_control_mode: true, max_bytes: max },
            testing::heap().alloc(),
        );
        let kind = tape.int_in(0..=2u8, "kind");
        let (inter, params, f): (&[u8], &[u16], u8) = match kind {
            0 => (b"", &[1000], b'p'),
            1 => (b"+", &[], b'q'),
            _ => (b"$", &[], b'q'),
        };
        let bytes = tape.vec(0..=128, "bytes", |t| *t.pick(ALPHABET, "byte").unwrap_or(&b'%'));
        h.hook(&Dcs { intermediates: inter, params, final_byte: f });
        for &b in &bytes {
            if let Some(DcsCommand::Tmux(n)) = h.put(b) {
                let held = match n {
                    Note::BlockEnd(d) | Note::BlockErr(d) => d.len(),
                    Note::Output { data, .. } => data.len(),
                    _ => 0,
                };
                assert!(held <= max, "a notification larger than the cap");
            }
        }
        if let Some(DcsCommand::XtGetTcap(x)) = h.unhook() {
            assert!(x.data().len() <= max);
            assert!(!x.data().iter().any(u8::is_ascii_lowercase));
        }
    });
}

#[test]
fn keys_rejoin_to_the_payload() {
    // `keys` is `split(';')` less a final empty piece: joining them back with `;` gives the
    // payload without one trailing `;`.
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let payload = tape.vec(0..=24, "payload", |t| *t.pick(b"A;", "b").unwrap_or(&b'A'));
        let mut h = handler();
        hook(&mut h, b"+", &[], b'q');
        put_all(&mut h, &payload);
        let got = keys(h.unhook());
        let joined = got.join(&b';');
        let want = payload.strip_suffix(b";").unwrap_or(&payload);
        assert_eq!(joined, want);
        assert!(got.len() <= payload.iter().filter(|&&b| b == b';').count() + 1);
    });
}
