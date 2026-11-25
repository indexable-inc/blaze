//! Replies: ghostty's encoder tests by name (device_attributes.zig, device_status.zig,
//! size_report.zig, modes.zig, Terminal.zig printAttributes, Source.zig xtgettcap map, the
//! reply halves of stream_terminal.zig), one test per vt-inventory.md §12 row this module
//! owns, and properties.

use std::vec::Vec as StdVec;

use blaze_own::{Alloc, Erased, Inline, Vec, testing};

use super::*;
use crate::dcs::{DcsCommand, DcsHandler, DcsOpts};
use crate::parser::Dcs;
use crate::style::{Flags, Rgb, Style};

fn buf() -> Vec<u8, Alloc<'static>> {
    Vec::new_in(testing::heap().alloc())
}

/// The bytes one encoder call appends.
fn bytes(f: impl FnOnce(Out<'_, '_>) -> Result<(), AllocFailure>) -> StdVec<u8> {
    let mut out = buf();
    f(&mut out).expect("the test heap grants");
    out.as_slice().to_vec()
}

#[track_caller]
fn eq(got: impl AsRef<[u8]>, want: &[u8]) {
    let got = got.as_ref();
    assert_eq!(got, want, "\n got \"{}\"\nwant \"{}\"", got.escape_ascii(), want.escape_ascii());
}

// --- device_attributes.zig ---

#[test]
fn primary_default() {
    // device_attributes.zig:174 "primary default"
    eq(bytes(|o| da1(o, Da1::LIB)), b"\x1b[?62;22c");
}

#[test]
fn primary_with_clipboard() {
    // device_attributes.zig:181 "primary with clipboard"
    eq(bytes(|o| da1(o, Da1::APP)), b"\x1b[?62;22;52c");
}

#[test]
fn primary_with_multiple_features() {
    // device_attributes.zig:188 "primary with multiple features"
    eq(bytes(|o| da1(o, Da1 { conformance: 64, features: &[1, 6, 22] })), b"\x1b[?64;1;6;22c");
}

#[test]
fn primary_no_features() {
    // device_attributes.zig:198 "primary no features"
    eq(bytes(|o| da1(o, Da1 { conformance: 1, features: &[] })), b"\x1b[?1c");
}

#[test]
fn secondary_default() {
    // device_attributes.zig:208 "secondary default"
    eq(bytes(|o| da2(o, Da2::LIB)), b"\x1b[>1;0;0c");
}

#[test]
fn tertiary_default() {
    // device_attributes.zig:215 "tertiary default"
    eq(bytes(|o| da3(o, 0)), b"\x1bP!|00000000\x1b\\");
}

#[test]
fn tertiary_custom_unit_id() {
    // device_attributes.zig:222 "tertiary custom unit id"
    eq(bytes(|o| da3(o, 0xAABB_CCDD)), b"\x1bP!|AABBCCDD\x1b\\");
}

#[test]
fn device_attributes_custom_response() {
    // stream_terminal.zig:2541 "device attributes: custom response"
    eq(bytes(|o| da1(o, Da1 { conformance: 64, features: &[22, 52] })), b"\x1b[?64;22;52c");
    eq(
        bytes(|o| da2(o, Da2 { device_type: 41, firmware: 100, rom_cartridge: 0 })),
        b"\x1b[>41;100;0c",
    );
}

// --- device_status.zig ---

#[test]
fn encode_color_scheme_report_dark() {
    // device_status.zig:102 "encode color scheme report dark"
    eq(bytes(|o| color_scheme(o, ColorScheme::Dark)), b"\x1b[?997;1n");
}

#[test]
fn encode_color_scheme_report_light() {
    // device_status.zig:111 "encode color scheme report light"
    eq(bytes(|o| color_scheme(o, ColorScheme::Light)), b"\x1b[?997;2n");
}

#[test]
fn device_status_operating_status() {
    // stream_terminal.zig:2249 "device status: operating status"
    eq(bytes(operating_status), b"\x1b[0n");
}

#[test]
fn device_status_cursor_position() {
    // stream_terminal.zig:2274 "device status: cursor position"
    let at = |x, y| CursorReport { x, y, origin: false, left: 0, top: 0 };
    eq(bytes(|o| cursor_position(o, at(0, 0))), b"\x1b[1;1R");
    eq(bytes(|o| cursor_position(o, at(9, 4))), b"\x1b[5;10R");
}

#[test]
fn device_status_cursor_position_with_origin_mode() {
    // stream_terminal.zig:2304 "device status: cursor position with origin mode": region rows
    // 5-20, `CSI 3;5H` in origin mode puts the cursor at row 6 (0-based), column 4.
    eq(
        bytes(|o| cursor_position(o, CursorReport { x: 4, y: 6, origin: true, left: 0, top: 4 })),
        b"\x1b[3;5R",
    );
    // Saturates at the region's corner (ST:388 `-|`).
    eq(
        bytes(|o| cursor_position(o, CursorReport { x: 0, y: 0, origin: true, left: 3, top: 4 })),
        b"\x1b[1;1R",
    );
}

// --- size_report.zig ---

const TEST_SIZE: Size = Size { rows: 24, columns: 80, cell_width: 9, cell_height: 18 };

#[test]
fn encode_mode_2048() {
    // size_report.zig:92 "encode mode 2048"
    eq(bytes(|o| size_report(o, SizeReport::Mode2048, TEST_SIZE)), b"\x1b[48;24;80;432;720t");
}

#[test]
fn encode_csi_14_t() {
    // size_report.zig:100 "encode csi 14 t"
    eq(bytes(|o| size_report(o, SizeReport::Csi14, TEST_SIZE)), b"\x1b[4;432;720t");
}

#[test]
fn encode_csi_16_t() {
    // size_report.zig:108 "encode csi 16 t"
    eq(bytes(|o| size_report(o, SizeReport::Csi16, TEST_SIZE)), b"\x1b[6;18;9t");
}

#[test]
fn encode_csi_18_t() {
    // size_report.zig:116 "encode csi 18 t"
    eq(bytes(|o| size_report(o, SizeReport::Csi18, TEST_SIZE)), b"\x1b[8;24;80t");
}

#[test]
fn encode_max_values_for_all_fields() {
    // size_report.zig:124 "encode max values for all fields"
    let max =
        Size { rows: u16::MAX, columns: u16::MAX, cell_width: u32::MAX, cell_height: u32::MAX };
    eq(
        bytes(|o| size_report(o, SizeReport::Mode2048, max)),
        b"\x1b[48;65535;65535;281470681677825;281470681677825t",
    );
    eq(
        bytes(|o| size_report(o, SizeReport::Csi14, max)),
        b"\x1b[4;281470681677825;281470681677825t",
    );
    eq(bytes(|o| size_report(o, SizeReport::Csi16, max)), b"\x1b[6;4294967295;4294967295t");
    eq(bytes(|o| size_report(o, SizeReport::Csi18, max)), b"\x1b[8;65535;65535t");
}

#[test]
fn size_report_csi_21_t_title() {
    // stream_terminal.zig:2145 "size report csi_21_t title"
    eq(bytes(|o| title_report(o, b"My Title")), b"\x1b]lMy Title\x1b\\");
    eq(bytes(|o| title_report(o, b"")), b"\x1b]l\x1b\\");
}

// --- modes.zig ---

#[test]
fn report_encode_dec_mode_set() {
    // modes.zig:356 "Report.encode DEC mode set"
    eq(
        bytes(|o| mode_report(o, ModeReport { mode: 1, ansi: false, state: ReportState::Set })),
        b"\x1b[?1;1$y",
    );
}

#[test]
fn report_encode_dec_mode_reset() {
    // modes.zig:364 "Report.encode DEC mode reset"
    eq(
        bytes(|o| mode_report(o, ModeReport { mode: 1, ansi: false, state: ReportState::Reset })),
        b"\x1b[?1;2$y",
    );
}

#[test]
fn report_encode_ansi_mode() {
    // modes.zig:372 "Report.encode ANSI mode"
    eq(
        bytes(|o| mode_report(o, ModeReport { mode: 4, ansi: true, state: ReportState::Set })),
        b"\x1b[4;1$y",
    );
}

#[test]
fn report_encode_not_recognized() {
    // modes.zig:380 "Report.encode not recognized"
    eq(
        bytes(|o| {
            mode_report(
                o,
                ModeReport { mode: 9999, ansi: false, state: ReportState::NotRecognized },
            )
        }),
        b"\x1b[?9999;0$y",
    );
}

#[test]
fn mode_report_truncates_to_fifteen_bits() {
    // ST:535: `@truncate(mode_raw)` into the u15 tag, and the permanent states' numbers.
    eq(
        bytes(|o| {
            mode_report(
                o,
                ModeReport { mode: 40000, ansi: false, state: ReportState::NotRecognized },
            )
        }),
        b"\x1b[?7232;0$y",
    );
    eq(
        bytes(|o| {
            mode_report(
                o,
                ModeReport { mode: 2027, ansi: false, state: ReportState::PermanentlySet },
            )
        }),
        b"\x1b[?2027;3$y",
    );
    eq(
        bytes(|o| {
            mode_report(
                o,
                ModeReport { mode: 3, ansi: false, state: ReportState::PermanentlyReset },
            )
        }),
        b"\x1b[?3;4$y",
    );
}

// --- Terminal.zig printAttributes, DECRQSS ---

fn with(f: impl FnOnce(&mut Style)) -> Style {
    let mut s = Style::default();
    f(&mut s);
    s
}

#[test]
fn terminal_print_attributes() {
    // Terminal.zig:12714 "Terminal: printAttributes"
    let p = |s: Style| bytes(|o| sgr_payload(o, &s));
    eq(p(with(|s| s.fg = Color::Rgb(Rgb { r: 1, g: 2, b: 3 }))), b"0;38:2::1:2:3");
    eq(
        p(with(|s| {
            s.flags.bold = true;
            s.bg = Color::Rgb(Rgb { r: 1, g: 2, b: 3 });
        })),
        b"0;1;48:2::1:2:3",
    );
    eq(
        p(with(|s| {
            s.flags = Flags {
                bold: true,
                faint: true,
                italic: true,
                underline: Underline::Single,
                blink: true,
                inverse: true,
                invisible: true,
                strikethrough: true,
                overline: false,
            };
            s.fg = Color::Rgb(Rgb { r: 100, g: 200, b: 255 });
            s.bg = Color::Rgb(Rgb { r: 101, g: 102, b: 103 });
        })),
        b"0;1;2;3;4;5;7;8;9;38:2::100:200:255;48:2::101:102:103",
    );
    eq(p(with(|s| s.flags.underline = Underline::Single)), b"0;4");
    eq(p(Style::default()), b"0");
}

#[test]
fn print_attributes_palette_colours_and_what_is_not_reported() {
    // T:3470: palette 0-7 as 3n/4n, 8-15 as 9n/10n, 16+ as 38:5:n/48:5:n; underline style,
    // overline and underline colour are not reported.
    let p = |s: Style| bytes(|o| sgr_payload(o, &s));
    eq(p(with(|s| (s.fg, s.bg) = (Color::Palette(1), Color::Palette(7)))), b"0;31;47");
    eq(p(with(|s| (s.fg, s.bg) = (Color::Palette(9), Color::Palette(15)))), b"0;91;107");
    eq(
        p(with(|s| (s.fg, s.bg) = (Color::Palette(16), Color::Palette(255)))),
        b"0;38:5:16;48:5:255",
    );
    eq(
        p(with(|s| {
            s.flags.underline = Underline::Curly;
            s.flags.overline = true;
            s.underline_color = Color::Palette(3);
        })),
        b"0;4",
    );
}

fn state(style: &Style) -> DecrqssState<'_> {
    DecrqssState {
        style,
        top: 0,
        bottom: 23,
        left: 0,
        right: 79,
        left_right_margins: false,
        cursor_shape: CursorShape::Block,
        cursor_blink: true,
    }
}

#[test]
fn inventory_12_decrqss_valid_and_m_payload() {
    // Rows "DECRQSS valid" and "DECRQSS m payload": `DCS 1 $ r 0;1;31 m ST`.
    let s = with(|s| {
        s.flags.bold = true;
        s.fg = Color::Palette(1);
    });
    eq(bytes(|o| decrqss(o, Decrqss::Sgr, &state(&s))), b"\x1bP1$r0;1;31m\x1b\\");
}

#[test]
fn inventory_12_decrqss_invalid() {
    // Row "DECRQSS invalid": `DCS 0 $ r ST`.
    let s = Style::default();
    eq(bytes(|o| decrqss(o, Decrqss::None, &state(&s))), b"\x1bP0$r\x1b\\");
}

#[test]
fn inventory_8_decrqss_decstbm() {
    // Row "DCS $ q r": `{t+1};{b+1}r`.
    let s = Style::default();
    eq(
        bytes(|o| decrqss(o, Decrqss::Decstbm, &DecrqssState { top: 4, bottom: 19, ..state(&s) })),
        b"\x1bP1$r5;20r\x1b\\",
    );
}

#[test]
fn inventory_8_decrqss_decslrm_only_with_mode_69() {
    // Row "DCS $ q s": `{l+1};{r+1}s`, only while DECLRMM is set, else invalid.
    let s = Style::default();
    let st = DecrqssState { left: 2, right: 9, ..state(&s) };
    eq(bytes(|o| decrqss(o, Decrqss::Decslrm, &st)), b"\x1bP0$r\x1b\\");
    let st = DecrqssState { left_right_margins: true, ..st };
    eq(bytes(|o| decrqss(o, Decrqss::Decslrm, &st)), b"\x1bP1$r3;10s\x1b\\");
}

#[test]
fn inventory_8_decrqss_decscusr_hollow_is_block() {
    // Row "DCS $ q SP q": `{1-6} q`; block_hollow is reported as block (SH:508).
    let s = Style::default();
    for (shape, blink, n) in [
        (CursorShape::Block, true, b'1'),
        (CursorShape::Block, false, b'2'),
        (CursorShape::Underline, true, b'3'),
        (CursorShape::Underline, false, b'4'),
        (CursorShape::Bar, true, b'5'),
        (CursorShape::Bar, false, b'6'),
        (CursorShape::BlockHollow, true, b'1'),
        (CursorShape::BlockHollow, false, b'2'),
    ] {
        let st = DecrqssState { cursor_shape: shape, cursor_blink: blink, ..state(&s) };
        let mut want = b"\x1bP1$r".to_vec();
        want.push(n);
        want.extend_from_slice(b" q\x1b\\");
        eq(bytes(|o| decrqss(o, Decrqss::Decscusr, &st)), &want);
    }
}

// --- XTGETTCAP: Source.zig and the real table ---

#[test]
fn xtgettcap_map() {
    // Source.zig:172 "xtgettcap map": its own little source, through the same encoding.
    eq(bytes(|o| xtgettcap_cap(o, "am", &Cap::Bool)), b"\x1bP1+r616D\x1b\\");
    eq(bytes(|o| xtgettcap_cap(o, "kx", &Cap::Str("^?"))), b"\x1bP1+r6B78=7F\x1b\\");
    eq(bytes(|o| xtgettcap_cap(o, "kbs", &Cap::Str("^H"))), b"\x1bP1+r6B6273=08\x1b\\");
    eq(bytes(|o| xtgettcap_cap(o, "kf1", &Cap::Str(r"\EOP"))), b"\x1bP1+r6B6631=1B4F50\x1b\\");
    eq(
        bytes(|o| xtgettcap_cap(o, "Smulx", &Cap::Str(r"\E[4:%p1%dm"))),
        b"\x1bP1+r536D756C78=5C455B343A25703125646D\x1b\\",
    );
    eq(bytes(|o| xtgettcap_cap(o, "colors", &Cap::Num(256))), b"\x1bP1+r636F6C6F7273=323536\x1b\\");
}

/// The replies a whole `DCS + q payload ST` gets, through the handler.
fn ask(payload: &[u8]) -> StdVec<u8> {
    let mut h = DcsHandler::new_in(
        DcsOpts { tmux_control_mode: false, max_bytes: DcsOpts::MAX_BYTES },
        testing::heap().alloc(),
    );
    h.hook(&Dcs { intermediates: b"+", params: &[], final_byte: b'q' });
    for &b in payload {
        h.put(b);
    }
    match h.unhook() {
        Some(DcsCommand::XtGetTcap(x)) => bytes(|o| xtgettcap(o, &x)),
        other => panic!("{other:?}"),
    }
}

fn hex(s: &[u8]) -> StdVec<u8> {
    s.iter().flat_map(|b| std::format!("{b:02X}").into_bytes()).collect()
}

#[test]
fn inventory_12_xtgettcap_extra_keys_tn_co_rgb() {
    // Row "DCS + q": extra keys TN, Co=256, RGB=8 (Source.zig:80).
    let mut want = StdVec::new();
    for (k, v) in [(&b"TN"[..], &b"xterm-ghostty"[..]), (b"Co", b"256"), (b"RGB", b"8")] {
        want.extend_from_slice(b"\x1bP1+r");
        want.extend(hex(k));
        want.push(b'=');
        want.extend(hex(v));
        want.extend_from_slice(b"\x1b\\");
    }
    eq(ask(b"544E;436f;524742"), &want);
}

#[test]
fn inventory_12_xtgettcap_booleans_strings_and_raw_percent() {
    // Row "XTGETTCAP": booleans have no `=`; strings without `%` have `\E` and `^X` decoded;
    // strings with `%` are raw. `cr` keeps its `\r` as two bytes (only `\E` and `^` decode).
    let mut want = StdVec::new();
    want.extend_from_slice(b"\x1bP1+r5463\x1b\\"); // Tc
    want.extend_from_slice(b"\x1bP1+r6B6273=7F\x1b\\"); // kbs=^?
    want.extend_from_slice(b"\x1bP1+r6B63757531=1B4F41\x1b\\"); // kcuu1=\EOA
    want.extend_from_slice(b"\x1bP1+r6372=5C72\x1b\\"); // cr=\r, raw
    want.extend_from_slice(b"\x1bP1+r5373=");
    want.extend(hex(br"\E[%p1%d q")); // Ss, raw: has `%`
    want.extend_from_slice(b"\x1b\\");
    want.extend_from_slice(b"\x1bP1+r727331=");
    want.extend(hex(b"\x1b]\x1b\\\\\x1bc")); // rs1 = \E]\E\\\Ec
    want.extend_from_slice(b"\x1b\\");
    let q: StdVec<u8> = [&b"Tc"[..], b"kbs", b"kcuu1", b"cr", b"Ss", b"rs1"]
        .iter()
        .map(|k| hex(k))
        .collect::<StdVec<_>>()
        .join(&b';');
    eq(ask(&q), &want);
}

#[test]
fn inventory_8_xtgettcap_unknown_keys_get_no_reply() {
    // Row "DCS + q": unknown keys get NO reply (SH:472 `orelse continue`), known ones around
    // them still do. (Not `DCS 0 + r`: ghostty sends nothing.)
    eq(ask(b"who;5A5A5A;"), b"");
    eq(ask(b""), b"");
    let mut want = b"\x1bP1+r616D\x1b\\".to_vec();
    want.extend_from_slice(b"\x1bP1+r616D\x1b\\");
    eq(ask(b"616D;6E6F7065;;616d"), &want);
}

#[test]
fn the_table_is_ghosttys_and_sorted() {
    // 268 capabilities in ghostty.zig plus TN, Co, RGB; sorted and unique so a lookup halves;
    // a `^` only leads a string without `%` (ghostty refuses others at compile time).
    assert_eq!(terminfo::GHOSTTY.len(), 268 + 3);
    for pair in terminfo::GHOSTTY.windows(2) {
        assert!(pair[0].0.as_bytes() < pair[1].0.as_bytes(), "{} before {}", pair[0].0, pair[1].0);
    }
    for (name, cap) in &terminfo::GHOSTTY {
        assert!(name.len() <= terminfo::MAX_NAME);
        assert_eq!(terminfo::lookup_hex(&hex(name.as_bytes())).map(|e| e.0), Some(*name));
        if let Cap::Str(s) = cap
            && !s.contains('%')
        {
            assert!(!s[1..].contains('^'), "{name}");
        }
    }
    assert_eq!(terminfo::NAMES[0], "xterm-ghostty");
    assert_eq!(terminfo::lookup_hex(b"616d"), None, "lower-case hex is not a key");
    assert_eq!(terminfo::lookup_hex(b"616"), None);
}

// --- XTVERSION, kitty, ENQ: stream_terminal.zig ---

#[test]
fn xtversion_default() {
    // stream_terminal.zig:1962 "xtversion default"
    eq(bytes(|o| xtversion(o, b"")), b"\x1bP>|libghostty\x1b\\");
}

#[test]
fn xtversion_with_effect() {
    // stream_terminal.zig:1985 "xtversion with effect"
    eq(bytes(|o| xtversion(o, b"ghostty 1.2.3")), b"\x1bP>|ghostty 1.2.3\x1b\\");
}

#[test]
fn xtversion_with_empty_string_effect() {
    // stream_terminal.zig:2011 "xtversion with empty string effect"
    eq(bytes(|o| xtversion(o, b"")), b"\x1bP>|libghostty\x1b\\");
}

#[test]
fn kitty_keyboard_query() {
    // stream_terminal.zig:1933 "kitty_keyboard_query"
    eq(bytes(|o| kitty_keyboard(o, 0)), b"\x1b[?0u");
    eq(bytes(|o| kitty_keyboard(o, 1)), b"\x1b[?1u");
    eq(bytes(|o| kitty_keyboard(o, 31)), b"\x1b[?31u");
}

#[test]
fn enquiry_with_effect() {
    // stream_terminal.zig:2195 "enquiry with effect"
    eq(bytes(|o| enquiry(o, b"ghostty")), b"ghostty");
}

#[test]
fn enquiry_with_empty_response() {
    // stream_terminal.zig:2222 "enquiry with empty response"
    eq(bytes(|o| enquiry(o, b"")), b"");
    // ST:420: 256 bytes or more do not fit libghostty's buffer and are not sent.
    eq(bytes(|o| enquiry(o, &[b'a'; 256])), b"");
    eq(bytes(|o| enquiry(o, &[b'a'; 255])), &[b'a'; 255]);
}

// --- OSC replies ---

#[test]
fn osc_color_query_responses() {
    // stream_terminal.zig:1273 "OSC color query responses"
    let c = |r, g, b| Rgb { r, g, b };
    eq(
        bytes(|o| {
            osc_color(
                o,
                OscColor::Palette(2),
                c(0x12, 0x34, 0x56),
                RgbFormat::Bits16,
                Terminator::St,
            )
        }),
        b"\x1b]4;2;rgb:1212/3434/5656\x1b\\",
    );
    let mut out = buf();
    osc_color(&mut out, OscColor::Foreground, c(1, 2, 3), RgbFormat::Bits16, Terminator::St)
        .unwrap();
    osc_color(&mut out, OscColor::Background, c(4, 5, 6), RgbFormat::Bits16, Terminator::St)
        .unwrap();
    osc_color(&mut out, OscColor::Cursor, c(7, 8, 9), RgbFormat::Bits16, Terminator::St).unwrap();
    eq(
        out.as_slice(),
        b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]12;rgb:0707/0808/0909\x1b\\",
    );
    // After OSC 112 the cursor falls back to the foreground; BEL echoes.
    eq(
        bytes(|o| osc_color(o, OscColor::Cursor, c(1, 2, 3), RgbFormat::Bits16, Terminator::Bel)),
        b"\x1b]12;rgb:0101/0202/0303\x07",
    );
}

#[test]
fn inventory_12_osc_4_eight_bit_format() {
    // Row "OSC 4 query": the app can be configured for 8-bit `rgb:rr/gg/bb` (SH:1416).
    let c = Rgb { r: 0xAB, g: 0x0C, b: 0xFF };
    eq(
        bytes(|o| osc_color(o, OscColor::Palette(255), c, RgbFormat::Bits8, Terminator::St)),
        b"\x1b]4;255;rgb:ab/0c/ff\x1b\\",
    );
    eq(
        bytes(|o| osc_color(o, OscColor::Background, c, RgbFormat::Bits16, Terminator::St)),
        b"\x1b]11;rgb:abab/0c0c/ffff\x1b\\",
    );
}

#[test]
fn kitty_color_protocol_query_responses() {
    // stream_terminal.zig:1424 "kitty color protocol query responses"
    eq(
        bytes(|o| {
            osc21(
                o,
                &[Osc21Entry { key: Osc21Key::Special("background"), color: None }],
                Terminator::St,
            )
        }),
        b"\x1b]21;background=\x1b\\",
    );
    eq(
        bytes(|o| {
            osc21(
                o,
                &[
                    Osc21Entry {
                        key: Osc21Key::Special("foreground"),
                        color: Some(Rgb { r: 0x12, g: 0x34, b: 0x56 }),
                    },
                    Osc21Entry { key: Osc21Key::Special("background"), color: None },
                    Osc21Entry {
                        key: Osc21Key::Palette(2),
                        color: Some(Rgb { r: 0xAA, g: 0xBB, b: 0xCC }),
                    },
                ],
                Terminator::St,
            )
        }),
        b"\x1b]21;foreground=rgb:12/34/56;background=;2=rgb:aa/bb/cc\x1b\\",
    );
    eq(bytes(|o| osc21(o, &[], Terminator::St)), b"");
}

#[test]
fn inventory_12_osc_52_read() {
    // Row "OSC 52 read": `OSC 52 ; kind ; b64 ST`, ST even if the query ended with BEL, and a
    // reply for empty contents too (Surface.zig:5945).
    eq(bytes(|o| osc52(o, Clipboard::Standard, b"aGk=")), b"\x1b]52;c;aGk=\x1b\\");
    eq(bytes(|o| osc52(o, Clipboard::Selection, b"")), b"\x1b]52;s;\x1b\\");
    eq(bytes(|o| osc52(o, Clipboard::Primary, b"eA==")), b"\x1b]52;p;eA==\x1b\\");
}

#[test]
fn inventory_12_osc_terminator_echo() {
    eq(bytes(|o| terminator(o, Terminator::St)), b"\x1b\\");
    eq(bytes(|o| terminator(o, Terminator::Bel)), b"\x07");
}

// --- the remaining §12 rows this module owns ---

#[test]
fn inventory_12_da1_app_and_lib() {
    eq(bytes(|o| da1(o, Da1::APP)), b"\x1b[?62;22;52c");
    eq(bytes(|o| da1(o, Da1::LIB)), b"\x1b[?62;22c");
}

#[test]
fn inventory_12_da2_app_and_lib() {
    eq(bytes(|o| da2(o, Da2::APP)), b"\x1b[>1;10;0c");
    eq(bytes(|o| da2(o, Da2::LIB)), b"\x1b[>1;0;0c");
}

#[test]
fn inventory_12_da3_lib() {
    eq(bytes(|o| da3(o, 0)), b"\x1bP!|00000000\x1b\\");
}

#[test]
fn inventory_12_dsr_5_and_cpr() {
    eq(bytes(operating_status), b"\x1b[0n");
    eq(
        bytes(|o| {
            cursor_position(o, CursorReport { x: 79, y: 23, origin: false, left: 0, top: 0 })
        }),
        b"\x1b[24;80R",
    );
}

#[test]
fn inventory_12_color_scheme_996_and_change() {
    eq(bytes(|o| color_scheme(o, ColorScheme::Dark)), b"\x1b[?997;1n");
    eq(bytes(|o| color_scheme(o, ColorScheme::Light)), b"\x1b[?997;2n");
}

#[test]
fn inventory_12_decrqm() {
    eq(
        bytes(|o| mode_report(o, ModeReport { mode: 7, ansi: false, state: ReportState::Set })),
        b"\x1b[?7;1$y",
    );
    eq(
        bytes(|o| mode_report(o, ModeReport { mode: 7, ansi: false, state: ReportState::Reset })),
        b"\x1b[?7;2$y",
    );
}

#[test]
fn inventory_12_xtversion_app_and_lib() {
    eq(bytes(|o| xtversion(o, b"ghostty 1.3.0")), b"\x1bP>|ghostty 1.3.0\x1b\\");
    eq(bytes(|o| xtversion(o, b"")), b"\x1bP>|libghostty\x1b\\");
}

#[test]
fn inventory_12_csi_t_and_2048() {
    eq(bytes(|o| size_report(o, SizeReport::Csi14, TEST_SIZE)), b"\x1b[4;432;720t");
    eq(bytes(|o| size_report(o, SizeReport::Csi16, TEST_SIZE)), b"\x1b[6;18;9t");
    eq(bytes(|o| size_report(o, SizeReport::Csi18, TEST_SIZE)), b"\x1b[8;24;80t");
    eq(bytes(|o| title_report(o, b"t")), b"\x1b]lt\x1b\\");
    eq(bytes(|o| size_report(o, SizeReport::Mode2048, TEST_SIZE)), b"\x1b[48;24;80;432;720t");
}

#[test]
fn inventory_12_kitty_keyboard_query() {
    eq(bytes(|o| kitty_keyboard(o, 5)), b"\x1b[?5u");
}

#[test]
fn inventory_12_osc_10_11_12_and_21() {
    let c = Rgb { r: 0, g: 0x80, b: 0xFF };
    eq(
        bytes(|o| osc_color(o, OscColor::Foreground, c, RgbFormat::Bits16, Terminator::Bel)),
        b"\x1b]10;rgb:0000/8080/ffff\x07",
    );
    eq(
        bytes(|o| {
            osc21(
                o,
                &[Osc21Entry { key: Osc21Key::Special("cursor"), color: Some(c) }],
                Terminator::Bel,
            )
        }),
        b"\x1b]21;cursor=rgb:00/80/ff\x07",
    );
}

#[test]
fn inventory_12_enq() {
    eq(bytes(|o| enquiry(o, b"\x06ack")), b"\x06ack");
}

// --- properties ---

#[test]
fn a_refused_reply_leaves_the_buffer_as_it_was() {
    // Every encoder appends all of its reply or none of it: under an allocator that runs out
    // at a drawn size, the buffer is the prefix written before, unchanged.
    blaze_sim::check(blaze_sim::Config::cases(300), |tape| {
        let inline = Inline::<256>::new();
        let erased = Erased::new(&inline);
        let mut out = Vec::new_in(erased.handle());
        let style = with(|s| {
            s.flags.bold = tape.coin("bold");
            s.fg = Color::Rgb(Rgb { r: 255, g: 255, b: 255 });
            s.bg = Color::Palette(tape.int_in(0..=255u8, "bg"));
        });
        let name_len = tape.int_in(0..=200usize, "title");
        let title = std::vec![b'x'; name_len];
        for _ in 0..tape.int_in(1..=12usize, "replies") {
            let before = out.as_slice().to_vec();
            let r = match tape.int_in(0..=4u8, "which") {
                0 => title_report(&mut out, &title),
                1 => decrqss(&mut out, Decrqss::Sgr, &state(&style)),
                2 => size_report(&mut out, SizeReport::Mode2048, TEST_SIZE),
                3 => osc_color(
                    &mut out,
                    OscColor::Palette(7),
                    Rgb { r: 1, g: 2, b: 3 },
                    RgbFormat::Bits16,
                    Terminator::St,
                ),
                _ => xtversion(&mut out, &title),
            };
            match r {
                Ok(()) => assert!(out.as_slice().starts_with(&before)),
                Err(_) => {
                    assert_eq!(out.as_slice(), &before[..], "a refusal wrote part of a reply")
                }
            }
        }
    });
}

#[test]
fn decimal_and_hex_match_std() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let rows = tape.int_in(0..=u16::MAX, "rows");
        let cols = tape.int_in(0..=u16::MAX, "cols");
        let cw = tape.int_in(0..=u32::MAX, "cw");
        let ch = tape.int_in(0..=u32::MAX, "ch");
        let s = Size { rows, columns: cols, cell_width: cw, cell_height: ch };
        let want = std::format!(
            "\x1b[48;{rows};{cols};{};{}t",
            u64::from(rows) * u64::from(ch),
            u64::from(cols) * u64::from(cw)
        );
        eq(bytes(|o| size_report(o, SizeReport::Mode2048, s)), want.as_bytes());
        let id = tape.int_in(0..=u32::MAX, "unit");
        eq(bytes(|o| da3(o, id)), std::format!("\x1bP!|{id:08X}\x1b\\").as_bytes());
        let (r, g, b) =
            (tape.int_in(0..=255u8, "r"), tape.int_in(0..=255u8, "g"), tape.int_in(0..=255u8, "b"));
        let want = std::format!(
            "\x1b]11;rgb:{:04x}/{:04x}/{:04x}\x1b\\",
            u16::from(r) * 257,
            u16::from(g) * 257,
            u16::from(b) * 257
        );
        eq(
            bytes(|o| {
                osc_color(
                    o,
                    OscColor::Background,
                    Rgb { r, g, b },
                    RgbFormat::Bits16,
                    Terminator::St,
                )
            }),
            want.as_bytes(),
        );
    });
}
