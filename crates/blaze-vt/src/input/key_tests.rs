//! ghostty's `src/input/key_encode.zig` tests, each under its ghostty name and line. ghostty
//! runs them on the build host's OS; the ones it skips off macOS run here as [`Platform::MacOs`]
//! ([`mac`]), and every other one runs as both platforms ([`both`]), as ghostty's CI does.

use super::*;
use crate::input::function_keys;
use std::vec::Vec;

const D: KittyFlags = KittyFlags::DISAMBIGUATE;
const EV: KittyFlags = KittyFlags::REPORT_EVENTS;
const ALT: KittyFlags = KittyFlags::REPORT_ALTERNATES;
const ALL: KittyFlags = KittyFlags::REPORT_ALL;
const ASSOC: KittyFlags = KittyFlags::REPORT_ASSOCIATED;

fn ev() -> KeyEvent<'static> {
    KeyEvent::default()
}

fn opts() -> EncodeOpts {
    EncodeOpts::new(Platform::Other)
}

fn kitty_opts(flags: KittyFlags) -> EncodeOpts {
    EncodeOpts { kitty_flags: flags, ..opts() }
}

fn enc(event: &KeyEvent<'_>, opts: &EncodeOpts) -> Vec<u8> {
    let mut buf = std::vec![0u8; max_len(event)];
    encode(event, opts, &mut buf).unwrap().to_vec()
}

fn on(platform: Platform, event: &KeyEvent<'_>, opts: &EncodeOpts) -> Vec<u8> {
    enc(event, &EncodeOpts { platform, ..*opts })
}

/// The encoding on both platforms equals `want`.
#[track_caller]
fn both(event: KeyEvent<'_>, opts: EncodeOpts, want: &[u8]) {
    for p in [Platform::MacOs, Platform::Other] {
        assert_eq!(on(p, &event, &opts), want, "{p:?} {event:?}");
    }
}

/// The encoding on macOS equals `want` (ghostty skips these elsewhere).
#[track_caller]
fn mac(event: KeyEvent<'_>, opts: EncodeOpts, want: &[u8]) {
    assert_eq!(on(Platform::MacOs, &event, &opts), want, "{event:?}");
}

fn seq(key: u32, final_byte: u8) -> KittySequence<'static> {
    KittySequence {
        key,
        final_byte,
        mods: KittyMods::default(),
        event: KittyEvent::None,
        alternates: [None, None],
        text: b"",
    }
}

fn run(s: KittySequence<'_>) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let mut w = Out::new(&mut buf);
    s.encode(&mut w).unwrap();
    w.finish().to_vec()
}

const KSHIFT: KittyMods = KittyMods {
    shift: true,
    alt: false,
    ctrl: false,
    super_key: false,
    hyper: false,
    meta: false,
    caps_lock: false,
    num_lock: false,
};

// ghostty key_encode.zig:868 "modifier sequence values" (CsiUMods)
#[test]
fn csi_u_mods_modifier_sequence_values() {
    let m = |shift, alt, ctrl| CsiUMods { shift, alt, ctrl }.seq_int();
    assert_eq!(m(false, false, false), 1);
    assert_eq!(m(true, false, false), 2);
    assert_eq!(m(false, true, false), 3);
    assert_eq!(m(false, false, true), 5);
    assert_eq!(m(true, true, false), 4);
    assert_eq!(m(true, false, true), 6);
    assert_eq!(m(false, true, true), 7);
    assert_eq!(m(true, true, true), 8);
}

// ghostty key_encode.zig:951 "modifier sequence values" (KittyMods)
#[test]
fn kitty_mods_modifier_sequence_values() {
    let m = |shift, alt, ctrl| KittyMods { shift, alt, ctrl, ..KittyMods::default() }.seq_int();
    assert_eq!(m(false, false, false), 1);
    assert_eq!(m(true, false, false), 2);
    assert_eq!(m(false, true, false), 3);
    assert_eq!(m(false, false, true), 5);
    assert_eq!(m(true, true, false), 4);
    assert_eq!(m(true, false, true), 6);
    assert_eq!(m(false, true, true), 7);
    assert_eq!(m(true, true, true), 8);
}

// ghostty key_encode.zig:1093 "KittySequence: backspace"
#[test]
fn kitty_sequence_backspace() {
    assert_eq!(run(seq(127, b'u')), b"\x1B[127u");
    assert_eq!(
        run(KittySequence { event: KittyEvent::Release, ..seq(127, b'u') }),
        b"\x1B[127;1:3u"
    );
    assert_eq!(run(KittySequence { mods: KSHIFT, ..seq(127, b'u') }), b"\x1B[127;2u");
}

// ghostty key_encode.zig:1125 "KittySequence: text"
#[test]
fn kitty_sequence_text() {
    assert_eq!(run(KittySequence { text: b"A", ..seq(127, b'u') }), b"\x1B[127;;65u");
    assert_eq!(
        run(KittySequence { event: KittyEvent::Release, text: b"A", ..seq(127, b'u') }),
        b"\x1B[127;1:3;65u"
    );
    assert_eq!(
        run(KittySequence { mods: KSHIFT, text: b"A", ..seq(127, b'u') }),
        b"\x1B[127;2;65u"
    );
}

// ghostty key_encode.zig:1167 "KittySequence: text with control characters"
#[test]
fn kitty_sequence_text_with_control_characters() {
    assert_eq!(run(KittySequence { text: b"\n", ..seq(127, b'u') }), b"\x1b[127u");
    assert_eq!(run(KittySequence { text: b"A\n", ..seq(127, b'u') }), b"\x1b[127;;65u");
}

// ghostty key_encode.zig:1195 "KittySequence: special no mods"
#[test]
fn kitty_sequence_special_no_mods() {
    assert_eq!(run(seq(1, b'A')), b"\x1B[A");
}

// ghostty key_encode.zig:1203 "KittySequence: special mods only"
#[test]
fn kitty_sequence_special_mods_only() {
    assert_eq!(run(KittySequence { mods: KSHIFT, ..seq(1, b'A') }), b"\x1B[1;2A");
}

// ghostty key_encode.zig:1211 "KittySequence: special mods and event"
#[test]
fn kitty_sequence_special_mods_and_event() {
    assert_eq!(
        run(KittySequence { event: KittyEvent::Release, mods: KSHIFT, ..seq(1, b'A') }),
        b"\x1B[1;2:3A"
    );
}

// ghostty key_encode.zig:1224 "kitty: plain text"
#[test]
fn kitty_plain_text() {
    let e = KeyEvent { key: Key::KeyA, utf8: b"abcd", ..ev() };
    both(e, kitty_opts(D), b"abcd");
}

// ghostty key_encode.zig:1237 "kitty: repeat with just disambiguate"
#[test]
fn kitty_repeat_with_just_disambiguate() {
    let e = KeyEvent { key: Key::KeyA, action: Action::Repeat, utf8: b"a", ..ev() };
    both(e, kitty_opts(D), b"a");
}

// ghostty key_encode.zig:1251 "kitty: enter, backspace, tab"
#[test]
fn kitty_enter_backspace_tab() {
    let k = |key| KeyEvent { key, ..ev() };
    let rel = |key| KeyEvent { key, action: Action::Release, ..ev() };
    both(k(Key::Enter), kitty_opts(D), b"\r");
    // DECBKM reset, then set: kitty does not support DECBKM, so no change.
    both(k(Key::Backspace), EncodeOpts { backarrow_key_mode: false, ..kitty_opts(D) }, b"\x7f");
    both(k(Key::Backspace), EncodeOpts { backarrow_key_mode: true, ..kitty_opts(D) }, b"\x7f");
    both(k(Key::Tab), kitty_opts(D), b"\t");
    // No release events without "report all".
    both(rel(Key::Enter), kitty_opts(D | EV), b"");
    both(rel(Key::Backspace), kitty_opts(D | EV), b"");
    both(rel(Key::Tab), kitty_opts(D | EV), b"");
    // Release events with "report all".
    both(rel(Key::Enter), kitty_opts(D | EV | ALL), b"\x1b[13;1:3u");
    both(rel(Key::Backspace), kitty_opts(D | EV | ALL), b"\x1b[127;1:3u");
    both(rel(Key::Tab), kitty_opts(D | EV | ALL), b"\x1b[9;1:3u");
}

// ghostty key_encode.zig:1345 "kitty: shift+backspace emits CSI u"
#[test]
fn kitty_shift_backspace_emits_csi_u() {
    let e = KeyEvent { key: Key::Backspace, mods: Mods::SHIFT, ..ev() };
    both(e, kitty_opts(D), b"\x1b[127;2u");
}

// ghostty key_encode.zig:1360 "kitty: shift+enter emits CSI u"
#[test]
fn kitty_shift_enter_emits_csi_u() {
    let e = KeyEvent { key: Key::Enter, mods: Mods::SHIFT, ..ev() };
    both(e, kitty_opts(D), b"\x1b[13;2u");
}

// ghostty key_encode.zig:1373 "kitty: shift+tab emits CSI u"
#[test]
fn kitty_shift_tab_emits_csi_u() {
    let e = KeyEvent { key: Key::Tab, mods: Mods::SHIFT, ..ev() };
    both(e, kitty_opts(D), b"\x1b[9;2u");
}

// ghostty key_encode.zig:1386 "kitty: enter with all flags"
#[test]
fn kitty_enter_with_all_flags() {
    let e = KeyEvent { key: Key::Enter, ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), b"\x1b[13u");
}

// ghostty key_encode.zig:1402 "kitty: ctrl with all flags"
#[test]
fn kitty_ctrl_with_all_flags() {
    let e = KeyEvent { key: Key::ControlLeft, mods: Mods::CTRL, ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), b"\x1b[57442;5u");
}

// ghostty key_encode.zig:1418 "kitty: ctrl release with ctrl mod set"
#[test]
fn kitty_ctrl_release_with_ctrl_mod_set() {
    let e = KeyEvent { action: Action::Release, key: Key::ControlLeft, mods: Mods::CTRL, ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), b"\x1b[57442;5:3u");
}

// ghostty key_encode.zig:1439 "kitty: delete"
#[test]
fn kitty_delete() {
    let e = KeyEvent { key: Key::Delete, utf8: b"\x7F", ..ev() };
    both(e, kitty_opts(D), b"\x1b[3~");
}

// ghostty key_encode.zig:1450 "kitty: composing with no modifier"
#[test]
fn kitty_composing_with_no_modifier() {
    let e = KeyEvent { key: Key::KeyA, mods: Mods::SHIFT, composing: true, ..ev() };
    both(e, kitty_opts(D), b"");
}

// ghostty key_encode.zig:1463 "kitty: composing with modifier"
#[test]
fn kitty_composing_with_modifier() {
    let e = KeyEvent { key: Key::ShiftLeft, mods: Mods::SHIFT, composing: true, ..ev() };
    both(e, kitty_opts(D | ALL), b"\x1b[57441;2u");
}

// ghostty key_encode.zig:1476 "kitty: composed text with report all"
#[test]
fn kitty_composed_text_with_report_all() {
    let e = KeyEvent { key: Key::Unidentified, utf8: "û".as_bytes(), ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), "û".as_bytes());
}

// ghostty key_encode.zig:1495 "kitty: shift+a on US keyboard"
#[test]
fn kitty_shift_a_on_us_keyboard() {
    let e = KeyEvent {
        key: Key::KeyA,
        mods: Mods::SHIFT,
        utf8: b"A",
        unshifted_codepoint: Some('a'),
        ..ev()
    };
    both(e, kitty_opts(D | ALT), b"\x1b[97:65;2u");
}

// ghostty key_encode.zig:1512 "kitty: matching unshifted codepoint"
#[test]
fn kitty_matching_unshifted_codepoint() {
    // Not a valid encoding: a hypothetical one that checks the base-key alternate is given
    // when the unshifted code point is not the base key.
    let e = KeyEvent {
        key: Key::KeyA,
        mods: Mods::SHIFT,
        utf8: b"A",
        unshifted_codepoint: Some('A'),
        ..ev()
    };
    both(e, kitty_opts(D | ALT), b"\x1b[65::97;2u");
}

// ghostty key_encode.zig:1533 "kitty: report alternates with caps"
#[test]
fn kitty_report_alternates_with_caps() {
    let e = KeyEvent {
        key: Key::KeyJ,
        mods: Mods::CAPS_LOCK,
        utf8: b"J",
        unshifted_codepoint: Some('j'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[106;65;74u");
}

// ghostty key_encode.zig:1552 "kitty: report alternates colon (shift+';')"
#[test]
fn kitty_report_alternates_colon_shift_semicolon() {
    let e = KeyEvent {
        key: Key::Semicolon,
        mods: Mods::SHIFT,
        utf8: b":",
        unshifted_codepoint: Some(';'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[59:58;2;58u");
}

// ghostty key_encode.zig:1571 "kitty: report alternates with ru layout"
#[test]
fn kitty_report_alternates_with_ru_layout() {
    let e = KeyEvent {
        key: Key::Semicolon,
        utf8: "ч".as_bytes(),
        unshifted_codepoint: Some('ч'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[1095::59;;1095u");
}

// ghostty key_encode.zig:1590 "kitty: report alternates with ru layout shifted"
#[test]
fn kitty_report_alternates_with_ru_layout_shifted() {
    let e = KeyEvent {
        key: Key::Semicolon,
        mods: Mods::SHIFT,
        utf8: "Ч".as_bytes(),
        unshifted_codepoint: Some('ч'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[1095:1063:59;2;1063u");
}

// ghostty key_encode.zig:1609 "kitty: report alternates with ru layout caps lock"
#[test]
fn kitty_report_alternates_with_ru_layout_caps_lock() {
    let e = KeyEvent {
        key: Key::Semicolon,
        mods: Mods::CAPS_LOCK,
        utf8: "Ч".as_bytes(),
        unshifted_codepoint: Some('ч'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[1095::59;65;1063u");
}

// ghostty key_encode.zig:1628 "kitty: report alternates with hu layout release"
#[test]
fn kitty_report_alternates_with_hu_layout_release() {
    let e = KeyEvent {
        action: Action::Release,
        key: Key::BracketLeft,
        mods: Mods::CTRL,
        unshifted_codepoint: Some('ő'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC | EV), b"\x1b[337::91;5:3u");
}

// ghostty key_encode.zig:1651 "kitty: up arrow with utf8" (macOS sends text for arrows)
#[test]
fn kitty_up_arrow_with_utf8() {
    let e = KeyEvent { key: Key::ArrowUp, utf8: &[30], ..ev() };
    both(e, kitty_opts(D), b"\x1b[A");
}

// ghostty key_encode.zig:1664 "kitty: shift+tab"
#[test]
fn kitty_shift_tab() {
    let e = KeyEvent { key: Key::Tab, mods: Mods::SHIFT, ..ev() };
    both(e, kitty_opts(D | ALT), b"\x1b[9;2u");
}

// ghostty key_encode.zig:1677 "kitty: left shift"
#[test]
fn kitty_left_shift() {
    let e = KeyEvent { key: Key::ShiftLeft, ..ev() };
    both(e, kitty_opts(D | ALT), b"");
}

// ghostty key_encode.zig:1690 "kitty: left shift with report all"
#[test]
fn kitty_left_shift_with_report_all() {
    let e = KeyEvent { key: Key::ShiftLeft, ..ev() };
    both(e, kitty_opts(D | ALL), b"\x1b[57441u");
}

// ghostty key_encode.zig:1703 "kitty: report associated with alt text on macOS with option"
#[test]
fn kitty_report_associated_with_alt_text_on_macos_with_option() {
    let e = KeyEvent {
        key: Key::KeyW,
        mods: Mods::ALT,
        utf8: "∑".as_bytes(),
        unshifted_codepoint: Some('w'),
        ..ev()
    };
    let o =
        EncodeOpts { macos_option_as_alt: OptionAsAlt::False, ..kitty_opts(D | ALL | ALT | ASSOC) };
    mac(e, o, b"\x1b[119;3;8721u");
}

// ghostty key_encode.zig:1725 "kitty: report associated with alt text on macOS with alt"
#[test]
fn kitty_report_associated_with_alt_text_on_macos_with_alt() {
    let o =
        EncodeOpts { macos_option_as_alt: OptionAsAlt::True, ..kitty_opts(D | ALL | ALT | ASSOC) };
    let e = KeyEvent {
        key: Key::KeyW,
        mods: Mods::ALT,
        utf8: "∑".as_bytes(),
        unshifted_codepoint: Some('w'),
        ..ev()
    };
    // With alt.
    mac(e, o, b"\x1b[119;3u");
    // Without alt.
    mac(KeyEvent { mods: Mods::NONE, ..e }, o, b"\x1b[119;;8721u");
}

// ghostty key_encode.zig:1771 "kitty: report associated with modifiers"
#[test]
fn kitty_report_associated_with_modifiers() {
    let e = KeyEvent {
        key: Key::KeyJ,
        mods: Mods::CTRL,
        utf8: b"j",
        unshifted_codepoint: Some('j'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[106;5u");
}

// ghostty key_encode.zig:1790 "kitty: report associated"
#[test]
fn kitty_report_associated() {
    let e = KeyEvent {
        key: Key::KeyJ,
        mods: Mods::SHIFT,
        utf8: b"J",
        unshifted_codepoint: Some('j'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC), b"\x1b[106:74;2;74u");
}

// ghostty key_encode.zig:1809 "kitty: report associated on release"
#[test]
fn kitty_report_associated_on_release() {
    let e = KeyEvent {
        action: Action::Release,
        key: Key::KeyJ,
        mods: Mods::SHIFT,
        utf8: b"J",
        unshifted_codepoint: Some('j'),
        ..ev()
    };
    both(e, kitty_opts(D | ALL | ALT | ASSOC | EV), b"\x1b[106:74;2:3u");
}

// ghostty key_encode.zig:1831 "kitty: alternates omit control characters"
#[test]
fn kitty_alternates_omit_control_characters() {
    let e = KeyEvent { key: Key::Delete, utf8: &[0x7F], ..ev() };
    both(e, kitty_opts(D | ALT | ALL), b"\x1b[3~");
}

// ghostty key_encode.zig:1848 "kitty: enter with utf8 (dead key state)"
#[test]
fn kitty_enter_with_utf8_dead_key_state() {
    let e = KeyEvent { key: Key::Enter, utf8: b"A", unshifted_codepoint: Some('\r'), ..ev() };
    both(e, kitty_opts(D | ALT | ALL), b"A");
}

// ghostty key_encode.zig:1865 "kitty: keypad number"
#[test]
fn kitty_keypad_number() {
    let e = KeyEvent { key: Key::Numpad1, utf8: b"1", ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), b"\x1b[57400;;49u");
}

// ghostty key_encode.zig:1885 "kitty: backspace with utf8 (dead key state)"
#[test]
fn kitty_backspace_with_utf8_dead_key_state() {
    let e = KeyEvent { key: Key::Backspace, utf8: b"A", unshifted_codepoint: Some('\r'), ..ev() };
    both(e, kitty_opts(KittyFlags::ALL), b"");
}

// ghostty key_encode.zig:1904 "legacy: backspace with utf8 (dead key state)"
#[test]
fn legacy_backspace_with_utf8_dead_key_state() {
    let e = KeyEvent { key: Key::Backspace, utf8: b"A", unshifted_codepoint: Some('\r'), ..ev() };
    both(e, opts(), b"");
}

// ghostty key_encode.zig:1915 "kitty: backspace (DECBKM reset) (report_all: true)"
#[test]
fn kitty_backspace_decbkm_reset_report_all() {
    let e = KeyEvent { key: Key::Backspace, ..ev() };
    let o = EncodeOpts { backarrow_key_mode: false, ..kitty_opts(KittyFlags::ALL) };
    both(e, o, b"\x1b[127u");
}

// ghostty key_encode.zig:1933 "kitty: backspace (DECBKM set) (report_all: true)"
#[test]
fn kitty_backspace_decbkm_set_report_all() {
    // Kitty does not support DECBKM, so there is no difference.
    let e = KeyEvent { key: Key::Backspace, ..ev() };
    let o = EncodeOpts { backarrow_key_mode: true, ..kitty_opts(KittyFlags::ALL) };
    both(e, o, b"\x1b[127u");
}

// ghostty key_encode.zig:1952 "legacy: enter with utf8 (dead key state)"
#[test]
fn legacy_enter_with_utf8_dead_key_state() {
    let e = KeyEvent { key: Key::Enter, utf8: b"A", unshifted_codepoint: Some('\r'), ..ev() };
    both(e, opts(), b"A");
}

// ghostty key_encode.zig:1963 "legacy: esc with utf8 (dead key state)"
#[test]
fn legacy_esc_with_utf8_dead_key_state() {
    let e = KeyEvent { key: Key::Escape, utf8: b"A", unshifted_codepoint: Some('\r'), ..ev() };
    both(e, opts(), b"A");
}

// ghostty key_encode.zig:1974 "legacy: ctrl+shift+minus (underscore on US)"
#[test]
fn legacy_ctrl_shift_minus_underscore_on_us() {
    let e = KeyEvent { key: Key::Minus, mods: Mods::CTRL | Mods::SHIFT, utf8: b"_", ..ev() };
    both(e, opts(), b"\x1F");
}

// ghostty key_encode.zig:1985 "legacy: ctrl+alt+c"
#[test]
fn legacy_ctrl_alt_c() {
    let e = KeyEvent { key: Key::KeyC, mods: Mods::CTRL | Mods::ALT, utf8: b"c", ..ev() };
    both(e, opts(), b"\x1b\x03");
}

// ghostty key_encode.zig:1996 "legacy: alt+c"
#[test]
fn legacy_alt_c() {
    let e = KeyEvent { key: Key::KeyC, utf8: b"c", mods: Mods::ALT, ..ev() };
    let o = EncodeOpts { alt_esc_prefix: true, macos_option_as_alt: OptionAsAlt::True, ..opts() };
    both(e, o, b"\x1Bc");
}

// ghostty key_encode.zig:2010 "legacy: alt+e only unshifted"
#[test]
fn legacy_alt_e_only_unshifted() {
    let e = KeyEvent { key: Key::KeyE, unshifted_codepoint: Some('e'), mods: Mods::ALT, ..ev() };
    let o = EncodeOpts { alt_esc_prefix: true, macos_option_as_alt: OptionAsAlt::True, ..opts() };
    both(e, o, b"\x1Be");
}

// ghostty key_encode.zig:2024 "legacy: alt+x macos"
#[test]
fn legacy_alt_x_macos() {
    let e = KeyEvent {
        key: Key::KeyC,
        utf8: "≈".as_bytes(),
        unshifted_codepoint: Some('c'),
        mods: Mods::ALT,
        ..ev()
    };
    let o = EncodeOpts { alt_esc_prefix: true, macos_option_as_alt: OptionAsAlt::True, ..opts() };
    mac(e, o, b"\x1Bc");
}

// ghostty key_encode.zig:2041 "legacy: shift+alt+. macos"
#[test]
fn legacy_shift_alt_period_macos() {
    let e = KeyEvent {
        key: Key::Period,
        utf8: b">",
        unshifted_codepoint: Some('.'),
        mods: Mods::ALT | Mods::SHIFT,
        ..ev()
    };
    let o = EncodeOpts { alt_esc_prefix: true, macos_option_as_alt: OptionAsAlt::True, ..opts() };
    mac(e, o, b"\x1B>");
}

// ghostty key_encode.zig:2058 "legacy: alt+ф"
#[test]
fn legacy_alt_ef() {
    let e = KeyEvent { key: Key::KeyF, utf8: "ф".as_bytes(), mods: Mods::ALT, ..ev() };
    let o = EncodeOpts { alt_esc_prefix: true, ..opts() };
    both(e, o, "ф".as_bytes());
}

// ghostty key_encode.zig:2071 "legacy: ctrl+c"
#[test]
fn legacy_ctrl_c() {
    let e = KeyEvent { key: Key::KeyC, mods: Mods::CTRL, utf8: b"c", ..ev() };
    both(e, opts(), b"\x03");
}

// ghostty key_encode.zig:2082 "legacy: ctrl+space"
#[test]
fn legacy_ctrl_space() {
    let e = KeyEvent { key: Key::Space, mods: Mods::CTRL, utf8: b" ", ..ev() };
    both(e, opts(), b"\x00");
}

// ghostty key_encode.zig:2093 "legacy: ctrl+shift+backspace"
#[test]
fn legacy_ctrl_shift_backspace() {
    let e = KeyEvent { key: Key::Backspace, mods: Mods::CTRL | Mods::SHIFT, ..ev() };
    both(e, opts(), b"\x08");
}

fn decbkm(on: bool) -> EncodeOpts {
    EncodeOpts { backarrow_key_mode: on, ..opts() }
}

// ghostty key_encode.zig:2103 "legacy: backspace (DECBKM reset)"
#[test]
fn legacy_backspace_decbkm_reset() {
    let e = KeyEvent { key: Key::Backspace, ..ev() };
    both(e, decbkm(false), b"\x7f");
}

// ghostty key_encode.zig:2113 "legacy: backspace (DECBKM reset, with ctrl)"
#[test]
fn legacy_backspace_decbkm_reset_with_ctrl() {
    let e = KeyEvent { key: Key::Backspace, mods: Mods::CTRL, ..ev() };
    both(e, decbkm(false), b"\x08");
}

// ghostty key_encode.zig:2125 "legacy: backspace (DECBKM set)"
#[test]
fn legacy_backspace_decbkm_set() {
    let e = KeyEvent { key: Key::Backspace, ..ev() };
    both(e, decbkm(true), b"\x08");
}

// ghostty key_encode.zig:2135 "legacy: backspace (DECBKM set, with ctrl)"
#[test]
fn legacy_backspace_decbkm_set_with_ctrl() {
    let e = KeyEvent { key: Key::Backspace, mods: Mods::CTRL, ..ev() };
    both(e, decbkm(true), b"\x7f");
}

fn mok2() -> EncodeOpts {
    EncodeOpts { modify_other_keys_state_2: true, ..opts() }
}

// ghostty key_encode.zig:2147 "legacy: ctrl+shift+char with modify other state 2"
#[test]
fn legacy_ctrl_shift_char_with_modify_other_state_2() {
    let e = KeyEvent { key: Key::KeyH, mods: Mods::CTRL | Mods::SHIFT, utf8: b"H", ..ev() };
    both(e, mok2(), b"\x1b[27;6;72~");
}

// ghostty key_encode.zig:2160 "legacy: ctrl+shift+char with modify other state 2 and consumed mods"
#[test]
fn legacy_ctrl_shift_char_with_modify_other_state_2_and_consumed_mods() {
    let e = KeyEvent {
        key: Key::KeyH,
        mods: Mods::CTRL | Mods::SHIFT,
        consumed_mods: Mods::SHIFT,
        utf8: b"H",
        ..ev()
    };
    both(e, mok2(), b"\x1b[27;6;72~");
}

// ghostty key_encode.zig:2174 "legacy: alt+digit with modify other state 2"
#[test]
fn legacy_alt_digit_with_modify_other_state_2() {
    let e = KeyEvent { key: Key::Digit8, mods: Mods::ALT, utf8: b"8", ..ev() };
    let o = EncodeOpts { macos_option_as_alt: OptionAsAlt::True, ..mok2() };
    both(e, o, b"\x1b[27;3;56~");
}

// ghostty key_encode.zig:2189 "legacy: alt+digit with modify other state 2 and macos-option-as-alt = false"
#[test]
fn legacy_alt_digit_with_modify_other_state_2_and_macos_option_as_alt_false() {
    // `[` is option+8's common translation on European layouts.
    let e = KeyEvent {
        key: Key::Digit8,
        mods: Mods::ALT,
        consumed_mods: Mods::ALT,
        utf8: b"[",
        ..ev()
    };
    let o = EncodeOpts { macos_option_as_alt: OptionAsAlt::False, ..mok2() };
    mac(e, o, b"[");
}

// ghostty key_encode.zig:2205 "legacy: fixterm awkward letters"
#[test]
fn legacy_fixterm_awkward_letters() {
    let c = |key, utf8: &'static [u8]| KeyEvent { key, mods: Mods::CTRL, utf8, ..ev() };
    both(c(Key::KeyI, b"i"), opts(), b"\x1b[105;5u");
    both(c(Key::KeyM, b"m"), opts(), b"\x1b[109;5u");
    both(c(Key::BracketLeft, b"["), opts(), b"\x1b[91;5u");
    let at = KeyEvent {
        key: Key::Digit2,
        mods: Mods::CTRL | Mods::SHIFT,
        utf8: b"@",
        unshifted_codepoint: Some('2'),
        ..ev()
    };
    both(at, opts(), b"\x1b[64;5u");
}

// ghostty key_encode.zig:2248 "legacy: ctrl+shift+letter ascii" (kitty's choice: the
// unshifted letter with shift)
#[test]
fn legacy_ctrl_shift_letter_ascii() {
    let e = KeyEvent {
        key: Key::KeyM,
        mods: Mods::CTRL | Mods::SHIFT,
        utf8: b"M",
        unshifted_codepoint: Some('m'),
        ..ev()
    };
    both(e, opts(), b"\x1b[109;6u");
}

// ghostty key_encode.zig:2262 "legacy: shift+function key should use all mods"
#[test]
fn legacy_shift_function_key_should_use_all_mods() {
    let e = KeyEvent { key: Key::ArrowUp, mods: Mods::SHIFT, consumed_mods: Mods::SHIFT, ..ev() };
    both(e, opts(), b"\x1b[1;2A");
}

// ghostty key_encode.zig:2273 "legacy: keypad enter"
#[test]
fn legacy_keypad_enter() {
    let e = KeyEvent { key: Key::NumpadEnter, ..ev() };
    both(e, opts(), b"\r");
}

fn kp1(mods: Mods) -> KeyEvent<'static> {
    KeyEvent { key: Key::Numpad1, mods, utf8: b"1", ..ev() }
}

// ghostty key_encode.zig:2284 "legacy: keypad 1"
#[test]
fn legacy_keypad_1() {
    both(kp1(Mods::NONE), opts(), b"1");
}

// ghostty key_encode.zig:2296 "legacy: keypad 1 with application keypad"
#[test]
fn legacy_keypad_1_with_application_keypad() {
    let o = EncodeOpts { keypad_key_application: true, ..opts() };
    both(kp1(Mods::NONE), o, b"\x1bOq");
}

// ghostty key_encode.zig:2310 "legacy: keypad 1 with application keypad and numlock"
#[test]
fn legacy_keypad_1_with_application_keypad_and_numlock() {
    let o = EncodeOpts { keypad_key_application: true, ..opts() };
    both(kp1(Mods::NUM_LOCK), o, b"\x1bOq");
}

// ghostty key_encode.zig:2324 "legacy: keypad 1 with application keypad and numlock ignore"
#[test]
fn legacy_keypad_1_with_application_keypad_and_numlock_ignore() {
    let o = EncodeOpts { keypad_key_application: true, ignore_keypad_with_numlock: true, ..opts() };
    both(kp1(Mods::NONE), o, b"1");
}

// ghostty key_encode.zig:2339 "legacy: f1"
#[test]
fn legacy_f1() {
    let c = |key| KeyEvent { key, mods: Mods::CTRL, ..ev() };
    both(c(Key::F1), opts(), b"\x1b[1;5P");
    both(c(Key::F2), opts(), b"\x1b[1;5Q");
    both(c(Key::F3), opts(), b"\x1b[13;5~");
    both(c(Key::F4), opts(), b"\x1b[1;5S");
    both(c(Key::F5), opts(), b"\x1b[15;5~");
}

// ghostty key_encode.zig:2398 "legacy: left_shift+tab"
#[test]
fn legacy_left_shift_tab() {
    let e = KeyEvent { key: Key::Tab, mods: Mods::SHIFT, ..ev() };
    both(e, opts(), b"\x1b[Z");
}

// ghostty key_encode.zig:2411 "legacy: right_shift+tab"
#[test]
fn legacy_right_shift_tab() {
    let e = KeyEvent { key: Key::Tab, mods: Mods::SHIFT | Mods::SHIFT_RIGHT_SIDE, ..ev() };
    both(e, opts(), b"\x1b[Z");
}

// ghostty key_encode.zig:2424 "legacy: hu layout ctrl+ő sends proper codepoint"
#[test]
fn legacy_hu_layout_ctrl_o_double_acute_sends_proper_codepoint() {
    let e = KeyEvent {
        key: Key::BracketLeft,
        mods: Mods::CTRL,
        utf8: "ő".as_bytes(),
        unshifted_codepoint: Some('ő'),
        ..ev()
    };
    both(e, opts(), b"\x1b[337;5u");
}

// ghostty key_encode.zig:2437 "legacy: super-only on macOS with text"
#[test]
fn legacy_super_only_on_macos_with_text() {
    let e = KeyEvent { key: Key::KeyB, utf8: b"b", mods: Mods::SUPER, ..ev() };
    mac(e, opts(), b"");
    // Elsewhere super+b types "b" (GNOME Console).
    assert_eq!(on(Platform::Other, &e, &opts()), b"b");
}

// ghostty key_encode.zig:2450 "legacy: super and other mods on macOS with text"
#[test]
fn legacy_super_and_other_mods_on_macos_with_text() {
    let e = KeyEvent { key: Key::KeyB, utf8: b"B", mods: Mods::SUPER | Mods::SHIFT, ..ev() };
    mac(e, opts(), b"");
}

// ghostty key_encode.zig:2463 "legacy: backspace with DEL utf8 (DECBKM reset)"
#[test]
fn legacy_backspace_with_del_utf8_decbkm_reset() {
    let e =
        KeyEvent { key: Key::Backspace, utf8: &[0x7F], unshifted_codepoint: Some('\x08'), ..ev() };
    both(e, decbkm(false), b"\x7F");
}

// ghostty key_encode.zig:2474 "legacy: backspace with DEL utf8 (DECBKM set)"
#[test]
fn legacy_backspace_with_del_utf8_decbkm_set() {
    let e =
        KeyEvent { key: Key::Backspace, utf8: &[0x7F], unshifted_codepoint: Some('\x08'), ..ev() };
    both(e, decbkm(true), b"\x08");
}

// ghostty key_encode.zig:2485 "ctrlseq: normal ctrl c"
#[test]
fn ctrlseq_normal_ctrl_c() {
    assert_eq!(ctrl_seq(Key::Unidentified, b"c", Some('c'), Mods::CTRL), Some(0x03));
}

// ghostty key_encode.zig:2490 "ctrlseq: normal ctrl c, right control"
#[test]
fn ctrlseq_normal_ctrl_c_right_control() {
    assert_eq!(
        ctrl_seq(Key::Unidentified, b"c", Some('c'), Mods::CTRL | Mods::CTRL_RIGHT_SIDE),
        Some(0x03)
    );
}

// ghostty key_encode.zig:2495 "ctrlseq: alt should be allowed"
#[test]
fn ctrlseq_alt_should_be_allowed() {
    assert_eq!(ctrl_seq(Key::Unidentified, b"c", Some('c'), Mods::ALT | Mods::CTRL), Some(0x03));
}

// ghostty key_encode.zig:2500 "ctrlseq: no ctrl does nothing"
#[test]
fn ctrlseq_no_ctrl_does_nothing() {
    assert_eq!(ctrl_seq(Key::Unidentified, b"c", Some('c'), Mods::NONE), None);
}

// ghostty key_encode.zig:2504 "ctrlseq: shifted non-character"
#[test]
fn ctrlseq_shifted_non_character() {
    assert_eq!(ctrl_seq(Key::Unidentified, b"_", Some('-'), Mods::CTRL | Mods::SHIFT), Some(0x1F));
}

// ghostty key_encode.zig:2509 "ctrlseq: caps ascii letter"
#[test]
fn ctrlseq_caps_ascii_letter() {
    assert_eq!(
        ctrl_seq(Key::Unidentified, b"C", Some('c'), Mods::CTRL | Mods::CAPS_LOCK),
        Some(0x03)
    );
}

// ghostty key_encode.zig:2514 "ctrlseq: shift does not generate ctrl seq"
#[test]
fn ctrlseq_shift_does_not_generate_ctrl_seq() {
    assert_eq!(ctrl_seq(Key::Unidentified, b"C", Some('c'), Mods::SHIFT), None);
    assert_eq!(ctrl_seq(Key::Unidentified, b"C", Some('c'), Mods::SHIFT | Mods::CTRL), None);
}

// ghostty key_encode.zig:2519 "ctrlseq: russian ctrl c"
#[test]
fn ctrlseq_russian_ctrl_c() {
    assert_eq!(ctrl_seq(Key::KeyC, "с".as_bytes(), Some('\u{441}'), Mods::CTRL), Some(0x03));
}

// ghostty key_encode.zig:2524 "ctrlseq: russian shifted ctrl c"
#[test]
fn ctrlseq_russian_shifted_ctrl_c() {
    assert_eq!(
        ctrl_seq(Key::KeyC, "с".as_bytes(), Some('\u{441}'), Mods::CTRL | Mods::SHIFT),
        None
    );
}

// ghostty key_encode.zig:2529 "ctrlseq: russian alt ctrl c"
#[test]
fn ctrlseq_russian_alt_ctrl_c() {
    assert_eq!(
        ctrl_seq(Key::KeyC, "с".as_bytes(), Some('\u{441}'), Mods::CTRL | Mods::ALT),
        Some(0x03)
    );
}

// ghostty key_encode.zig:2534 "ctrlseq: right ctrl c"
#[test]
fn ctrlseq_right_ctrl_c() {
    assert_eq!(
        ctrl_seq(Key::KeyC, "с".as_bytes(), Some('c'), Mods::CTRL | Mods::CTRL_RIGHT_SIDE),
        Some(0x03)
    );
}

// ghostty function_keys.zig:298 "keys": every sequence fits termio's small write (38 bytes).
#[test]
fn function_keys_keys() {
    for &key in function_keys::KEYS_WITH_ENTRIES {
        let entries = function_keys::entries(key);
        assert!(!entries.is_empty(), "{key:?}");
        for e in entries {
            // A modifier run's widest parameter (16) replaces the two-byte hole.
            assert!(e.sequence.len() <= 38, "{key:?}");
            assert!(e.decbkm.is_none_or(|d| d.len() <= 38), "{key:?}");
            let hole = e.sequence.windows(2).any(|w| w == b"{}");
            assert_eq!(hole, e.mods == function_keys::ModsMatch::EachModifier, "{key:?}");
        }
    }
}

// Beyond ghostty's tests.

/// Every modifier set on every legacy function key gives the `pcStyle` expansion ghostty's
/// comptime builds: `{}` replaced by the set's index in `function_keys.modifiers` plus two.
#[test]
fn pc_style_runs_expand_like_ghostty() {
    // ghostty function_keys.zig:53 `modifiers`, in order.
    let modifiers = [
        Mods::SHIFT,
        Mods::ALT,
        Mods::SHIFT | Mods::ALT,
        Mods::CTRL,
        Mods::SHIFT | Mods::CTRL,
        Mods::ALT | Mods::CTRL,
        Mods::SHIFT | Mods::ALT | Mods::CTRL,
        Mods::SUPER,
        Mods::SHIFT | Mods::SUPER,
        Mods::ALT | Mods::SUPER,
        Mods::SHIFT | Mods::ALT | Mods::SUPER,
        Mods::CTRL | Mods::SUPER,
        Mods::SHIFT | Mods::CTRL | Mods::SUPER,
        Mods::ALT | Mods::CTRL | Mods::SUPER,
        Mods::SHIFT | Mods::ALT | Mods::CTRL | Mods::SUPER,
    ];
    let cases: &[(Key, &str)] = &[
        (Key::ArrowUp, "\x1b[1;{}A"),
        (Key::ArrowDown, "\x1b[1;{}B"),
        (Key::ArrowRight, "\x1b[1;{}C"),
        (Key::ArrowLeft, "\x1b[1;{}D"),
        (Key::Home, "\x1b[1;{}H"),
        (Key::End, "\x1b[1;{}F"),
        (Key::Insert, "\x1b[2;{}~"),
        (Key::Delete, "\x1b[3;{}~"),
        (Key::PageUp, "\x1b[5;{}~"),
        (Key::PageDown, "\x1b[6;{}~"),
        (Key::F1, "\x1b[1;{}P"),
        (Key::F2, "\x1b[1;{}Q"),
        (Key::F3, "\x1b[13;{}~"),
        (Key::F4, "\x1b[1;{}S"),
        (Key::F5, "\x1b[15;{}~"),
        (Key::F6, "\x1b[17;{}~"),
        (Key::F7, "\x1b[18;{}~"),
        (Key::F8, "\x1b[19;{}~"),
        (Key::F9, "\x1b[20;{}~"),
        (Key::F10, "\x1b[21;{}~"),
        (Key::F11, "\x1b[23;{}~"),
        (Key::F12, "\x1b[24;{}~"),
        (Key::NumpadBegin, "\x1b[1;{}E"),
    ];
    for (i, &m) in modifiers.iter().enumerate() {
        assert_eq!(m.legacy_param(), Some(i as u8 + 2));
        for &(key, fmt) in cases {
            let want = fmt.replace("{}", &std::format!("{}", i + 2));
            for cursor_app in [false, true] {
                let e = KeyEvent { key, mods: m, ..ev() };
                let o = EncodeOpts { cursor_key_application: cursor_app, ..opts() };
                assert_eq!(on(Platform::Other, &e, &o), want.as_bytes(), "{key:?} {m:?}");
            }
        }
    }
}

#[test]
fn cursor_and_keypad_modes() {
    let k = |key| KeyEvent { key, ..ev() };
    let app = EncodeOpts { cursor_key_application: true, ..opts() };
    for (key, normal, application) in [
        (Key::ArrowUp, "\x1b[A", "\x1bOA"),
        (Key::NumpadDown, "\x1b[B", "\x1bOB"),
        (Key::Home, "\x1b[H", "\x1bOH"),
        (Key::NumpadEnd, "\x1b[F", "\x1bOF"),
        (Key::NumpadBegin, "\x1b[E", "\x1bOE"),
    ] {
        both(k(key), opts(), normal.as_bytes());
        both(k(key), app, application.as_bytes());
    }
    // Keypad application mode, modified: `SS3 m x`.
    let kpam = EncodeOpts { keypad_key_application: true, ..opts() };
    let e = KeyEvent { key: Key::NumpadAdd, mods: Mods::CTRL, utf8: b"+", ..ev() };
    both(e, kpam, b"\x1bO5k");
    both(KeyEvent { key: Key::NumpadEnter, ..ev() }, kpam, b"\x1bOM");
}

/// No event, under any options, outgrows [`max_len`].
#[test]
fn every_encoding_fits_max_len() {
    const KEYS: &[Key] = &[
        Key::KeyA,
        Key::KeyI,
        Key::Digit2,
        Key::Semicolon,
        Key::BracketLeft,
        Key::Backspace,
        Key::Enter,
        Key::Tab,
        Key::Escape,
        Key::Space,
        Key::ArrowUp,
        Key::F3,
        Key::F13,
        Key::F25,
        Key::Numpad1,
        Key::NumpadEnter,
        Key::NumpadBegin,
        Key::ShiftLeft,
        Key::MetaRight,
        Key::CapsLock,
        Key::Unidentified,
        Key::MediaPlayPause,
    ];
    const ALT_OPTS: [OptionAsAlt; 4] =
        [OptionAsAlt::False, OptionAsAlt::True, OptionAsAlt::Left, OptionAsAlt::Right];
    const ACTIONS: [Action; 3] = [Action::Press, Action::Release, Action::Repeat];
    const CHARS: &[char] = &['a', 'A', '\u{10FFFF}', 'ч', '\x7F', '\x1B', ' ', '\u{337}', '\r'];
    blaze_sim::check(blaze_sim::Config::cases(4000), |tape| {
        let text: std::string::String = tape
            .vec(0..=6, "text", |t| *t.pick(CHARS, "char").unwrap_or(&'a'))
            .into_iter()
            .collect();
        let mut bytes = text.into_bytes();
        if tape.coin("invalid") {
            bytes.push(0xFF);
        }
        let e = KeyEvent {
            action: *tape.pick(&ACTIONS, "action").unwrap_or(&Action::Press),
            key: *tape.pick(KEYS, "key").unwrap_or(&Key::KeyA),
            mods: Mods::from_bits(tape.int_in(0..=0x3FFu16, "mods")),
            consumed_mods: Mods::from_bits(tape.int_in(0..=0x3FFu16, "consumed")),
            composing: tape.below(8, "composing") == 0,
            utf8: &bytes,
            unshifted_codepoint: tape.pick(CHARS, "unshifted").copied(),
        };
        let o = EncodeOpts {
            platform: if tape.coin("mac") { Platform::MacOs } else { Platform::Other },
            cursor_key_application: tape.coin("decckm"),
            keypad_key_application: tape.coin("kpam"),
            backarrow_key_mode: tape.coin("decbkm"),
            ignore_keypad_with_numlock: tape.coin("1035"),
            alt_esc_prefix: tape.coin("1036"),
            modify_other_keys_state_2: tape.coin("mok2"),
            kitty_flags: KittyFlags::from_bits(tape.int_in(0..=31u8, "kitty")),
            macos_option_as_alt: *tape.pick(&ALT_OPTS, "option").unwrap_or(&OptionAsAlt::False),
        };
        let mut buf = std::vec![0u8; max_len(&e)];
        assert!(encode(&e, &o, &mut buf).is_ok(), "{e:?} {o:?}");
    });
}

/// A buffer one byte short fails whole, never with a truncated sequence.
#[test]
fn a_short_buffer_is_refused() {
    let e = KeyEvent { key: Key::ArrowUp, mods: Mods::CTRL, ..ev() };
    let mut buf = [0u8; 5];
    assert_eq!(encode(&e, &opts(), &mut buf), Err(BufferFull { capacity: 5 }));
    let mut buf = [0u8; 6];
    assert_eq!(encode(&e, &opts(), &mut buf).unwrap(), b"\x1b[1;5A");
}
