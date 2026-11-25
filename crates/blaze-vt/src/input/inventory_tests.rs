//! vt-inventory.md §10 (Input encoding): one test per row, named for it, through the public API
//! only.

use super::focus::{self, FocusEvent, FocusOpts};
use super::key::{
    self, Action, EncodeOpts, Key, KeyEvent, KittyFlags, Mods, OptionAsAlt, Platform,
};
use super::mouse::{
    self, Cell, MouseAction, MouseButton, MouseEvent, MouseEvents, MouseFormat, MouseOpts, Padding,
    Pixels, Size, SurfacePos,
};
use super::paste::{self, PasteOpts};
use std::vec::Vec;

const BOTH: [Platform; 2] = [Platform::MacOs, Platform::Other];

fn opts() -> EncodeOpts {
    EncodeOpts::new(Platform::Other)
}

fn kitty(flags: KittyFlags) -> EncodeOpts {
    EncodeOpts { kitty_flags: flags, ..opts() }
}

fn k(key: Key, mods: Mods) -> KeyEvent<'static> {
    KeyEvent { key, mods, ..KeyEvent::default() }
}

fn text(key: Key, mods: Mods, utf8: &'static str) -> KeyEvent<'static> {
    KeyEvent { key, mods, utf8: utf8.as_bytes(), ..KeyEvent::default() }
}

fn enc(e: &KeyEvent<'_>, o: &EncodeOpts) -> Vec<u8> {
    let mut buf = std::vec![0u8; key::max_len(e)];
    key::encode(e, o, &mut buf).unwrap().to_vec()
}

#[track_caller]
fn is(e: KeyEvent<'_>, o: EncodeOpts, want: &str) {
    for platform in BOTH {
        assert_eq!(enc(&e, &EncodeOpts { platform, ..o }), want.as_bytes(), "{platform:?} {e:?}");
    }
}

const S: Mods = Mods::SHIFT;
const A: Mods = Mods::ALT;
const C: Mods = Mods::CTRL;
const U: Mods = Mods::SUPER;

// "Modifier values": legacy parameter 2 S, 3 A, 4 SA, 5 C, 6 SC, 7 AC, 8 SAC, 9 Super, … 16.
#[test]
fn inv10_legacy_modifier_param() {
    let sets = [
        (S, 2),
        (A, 3),
        (S | A, 4),
        (C, 5),
        (S | C, 6),
        (A | C, 7),
        (S | A | C, 8),
        (U, 9),
        (S | U, 10),
        (A | U, 11),
        (S | A | U, 12),
        (C | U, 13),
        (S | C | U, 14),
        (A | C | U, 15),
        (S | A | C | U, 16),
    ];
    for (m, p) in sets {
        // Locks and sides do not change it.
        let m = m | Mods::CAPS_LOCK | Mods::NUM_LOCK | Mods::ALT_RIGHT_SIDE;
        is(k(Key::ArrowRight, m), opts(), &std::format!("\x1b[1;{p}C"));
    }
}

// "Modifier values": kitty 1 + bits (shift 1, alt 2, ctrl 4, super 8, caps 64, num 128).
#[test]
fn inv10_kitty_modifier_bits() {
    for (m, p) in [
        (S, 2),
        (A, 3),
        (C, 5),
        (U, 9),
        (Mods::CAPS_LOCK, 65),
        (Mods::NUM_LOCK, 129),
        (S | A | C | U | Mods::CAPS_LOCK | Mods::NUM_LOCK, 208),
    ] {
        is(k(Key::F13, m), kitty(KittyFlags::DISAMBIGUATE), &std::format!("\x1b[57376;{p}u"));
    }
}

// "Legacy keys": Up / Down / Right / Left, CSI A–D, `CSI 1;m X`, DECCKM SS3 A–D.
#[test]
fn inv10_legacy_arrows() {
    let app = EncodeOpts { cursor_key_application: true, ..opts() };
    for (key, f) in
        [(Key::ArrowUp, 'A'), (Key::ArrowDown, 'B'), (Key::ArrowRight, 'C'), (Key::ArrowLeft, 'D')]
    {
        is(k(key, Mods::NONE), opts(), &std::format!("\x1b[{f}"));
        is(k(key, Mods::NONE), app, &std::format!("\x1bO{f}"));
        is(k(key, S), opts(), &std::format!("\x1b[1;2{f}"));
        is(k(key, S), app, &std::format!("\x1b[1;2{f}"));
    }
}

// "Legacy keys": Home / End, CSI H / CSI F, `CSI 1;m H/F`, DECCKM SS3 H/F.
#[test]
fn inv10_legacy_home_end() {
    let app = EncodeOpts { cursor_key_application: true, ..opts() };
    for (key, f) in [(Key::Home, 'H'), (Key::End, 'F')] {
        is(k(key, Mods::NONE), opts(), &std::format!("\x1b[{f}"));
        is(k(key, Mods::NONE), app, &std::format!("\x1bO{f}"));
        is(k(key, C), opts(), &std::format!("\x1b[1;5{f}"));
    }
}

// "Legacy keys": Insert / Delete, PgUp / PgDn: `CSI n~`, `CSI n;m~`.
#[test]
fn inv10_legacy_insert_delete_pgup_pgdn() {
    for (key, n) in [(Key::Insert, 2), (Key::Delete, 3), (Key::PageUp, 5), (Key::PageDown, 6)] {
        is(k(key, Mods::NONE), opts(), &std::format!("\x1b[{n}~"));
        is(k(key, A), opts(), &std::format!("\x1b[{n};3~"));
    }
}

// "Legacy keys": F1–F4 SS3 P/Q/R/S; modified `CSI 1;m P`, `CSI 1;m Q`, `CSI 13;m~`, `CSI 1;m S`.
#[test]
fn inv10_legacy_f1_f4() {
    for (key, plain, modified) in [
        (Key::F1, "\x1bOP", "\x1b[1;2P"),
        (Key::F2, "\x1bOQ", "\x1b[1;2Q"),
        (Key::F3, "\x1bOR", "\x1b[13;2~"),
        (Key::F4, "\x1bOS", "\x1b[1;2S"),
    ] {
        is(k(key, Mods::NONE), opts(), plain);
        is(k(key, S), opts(), modified);
    }
}

// "Legacy keys": F5–F12 `CSI 15/17/18/19/20/21/23/24 ~`; F13 and above have no legacy encoding.
#[test]
fn inv10_legacy_f5_f12_and_no_f13() {
    let keys = [Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12];
    for (key, n) in keys.into_iter().zip([15, 17, 18, 19, 20, 21, 23, 24]) {
        is(k(key, Mods::NONE), opts(), &std::format!("\x1b[{n}~"));
        is(k(key, C | S), opts(), &std::format!("\x1b[{n};6~"));
    }
    for key in [Key::F13, Key::F20, Key::F25] {
        is(k(key, Mods::NONE), opts(), "");
        is(k(key, C), opts(), "");
    }
}

// "Legacy keys": keypad digits / Enter; application `SS3 p–y n o j m k M` or `SS3 m X`, only
// with ?66 set and ?1035 off.
#[test]
fn inv10_legacy_keypad() {
    let kpam = EncodeOpts { keypad_key_application: true, ..opts() };
    let numlock = EncodeOpts { ignore_keypad_with_numlock: true, ..kpam };
    let keys = [
        (Key::Numpad0, "0", 'p'),
        (Key::Numpad5, "5", 'u'),
        (Key::Numpad9, "9", 'y'),
        (Key::NumpadDecimal, ".", 'n'),
        (Key::NumpadDivide, "/", 'o'),
        (Key::NumpadMultiply, "*", 'j'),
        (Key::NumpadSubtract, "-", 'm'),
        (Key::NumpadAdd, "+", 'k'),
    ];
    for (key, t, f) in keys {
        is(text(key, Mods::NONE, t), opts(), t);
        is(text(key, Mods::NONE, t), kpam, &std::format!("\x1bO{f}"));
        is(text(key, S, t), kpam, &std::format!("\x1bO2{f}"));
        is(text(key, Mods::NONE, t), numlock, t);
    }
    is(k(Key::NumpadEnter, Mods::NONE), opts(), "\r");
    is(k(Key::NumpadEnter, Mods::NONE), kpam, "\x1bOM");
    is(k(Key::NumpadEnter, Mods::NONE), numlock, "\r");
}

// "Legacy keys": keypad arrows / Home / End / Begin / Ins / Del / PgUp / PgDn as the main keys;
// Begin is `CSI E` / `SS3 E`.
#[test]
fn inv10_legacy_keypad_navigation() {
    let app = EncodeOpts { cursor_key_application: true, ..opts() };
    for (kp, main) in [
        (Key::NumpadUp, Key::ArrowUp),
        (Key::NumpadDown, Key::ArrowDown),
        (Key::NumpadLeft, Key::ArrowLeft),
        (Key::NumpadRight, Key::ArrowRight),
        (Key::NumpadHome, Key::Home),
        (Key::NumpadEnd, Key::End),
        (Key::NumpadInsert, Key::Insert),
        (Key::NumpadDelete, Key::Delete),
        (Key::NumpadPageUp, Key::PageUp),
        (Key::NumpadPageDown, Key::PageDown),
    ] {
        for o in [opts(), app] {
            for m in [Mods::NONE, C] {
                assert_eq!(enc(&k(kp, m), &o), enc(&k(main, m), &o), "{kp:?}");
            }
        }
    }
    is(k(Key::NumpadBegin, Mods::NONE), opts(), "\x1b[E");
    is(k(Key::NumpadBegin, Mods::NONE), app, "\x1bOE");
}

// "Legacy keys": Backspace 0x7F (0x08 with DECBKM); ctrl 0x08 (0x7F with DECBKM); alt ESC 0x7F;
// modifyOtherKeys 2 `CSI 27;m;127~`.
#[test]
fn inv10_legacy_backspace() {
    let bkm = EncodeOpts { backarrow_key_mode: true, ..opts() };
    let mok2 = EncodeOpts { modify_other_keys_state_2: true, ..opts() };
    is(k(Key::Backspace, Mods::NONE), opts(), "\x7f");
    is(k(Key::Backspace, Mods::NONE), bkm, "\x08");
    is(k(Key::Backspace, C), opts(), "\x08");
    is(k(Key::Backspace, C), bkm, "\x7f");
    is(k(Key::Backspace, A), opts(), "\x1b\x7f");
    is(k(Key::Backspace, A), mok2, "\x1b[27;3;127~");
    is(k(Key::Backspace, S | A | C | U), mok2, "\x1b[27;16;127~");
}

// "Legacy keys": Tab `\t`; shift `CSI Z`; alt `ESC \t`; others `CSI 27;m;9~`.
#[test]
fn inv10_legacy_tab() {
    is(k(Key::Tab, Mods::NONE), opts(), "\t");
    is(k(Key::Tab, S), opts(), "\x1b[Z");
    is(k(Key::Tab, A), opts(), "\x1b\t");
    is(k(Key::Tab, C), opts(), "\x1b[27;5;9~");
    is(k(Key::Tab, S | A), opts(), "\x1b[27;4;9~");
}

// "Legacy keys": Enter `\r`; shift `CSI 27;2;13~`; alt `ESC \r`; others `CSI 27;m;13~`.
#[test]
fn inv10_legacy_enter() {
    is(k(Key::Enter, Mods::NONE), opts(), "\r");
    is(k(Key::Enter, S), opts(), "\x1b[27;2;13~");
    is(k(Key::Enter, A), opts(), "\x1b\r");
    is(k(Key::Enter, C), opts(), "\x1b[27;5;13~");
}

// "Legacy keys": Escape `ESC`; alt `ESC ESC`; others `CSI 27;m;27~`.
#[test]
fn inv10_legacy_escape() {
    is(k(Key::Escape, Mods::NONE), opts(), "\x1b");
    is(k(Key::Escape, A), opts(), "\x1b\x1b");
    is(k(Key::Escape, S), opts(), "\x1b[27;2;27~");
    is(k(Key::Escape, C), opts(), "\x1b[27;5;27~");
}

// "Legacy text": ctrl + char by kitty's C0 table; ctrl+0/1/9 send the digit; i, m, [ go to CSI u.
#[test]
fn inv10_ctrl_char_c0_table() {
    let cases = [
        (" ", Key::Space, 0u8),
        ("@", Key::Unidentified, 0),
        ("2", Key::Digit2, 0),
        ("3", Key::Digit3, 0x1b),
        ("4", Key::Digit4, 0x1c),
        ("5", Key::Digit5, 0x1d),
        ("6", Key::Digit6, 0x1e),
        ("~", Key::Unidentified, 0x1e),
        ("^", Key::Unidentified, 0x1e),
        ("7", Key::Digit7, 0x1f),
        ("/", Key::Slash, 0x1f),
        ("_", Key::Unidentified, 0x1f),
        ("8", Key::Digit8, 0x7f),
        ("?", Key::Unidentified, 0x7f),
        ("0", Key::Digit0, b'0'),
        ("1", Key::Digit1, b'1'),
        ("9", Key::Digit9, b'9'),
        ("\\", Key::Backslash, 0x1c),
        ("]", Key::BracketRight, 0x1d),
    ];
    for (t, key, b) in cases {
        is(text(key, C, t), opts(), core::str::from_utf8(&[b]).unwrap());
    }
    for c in b'a'..=b'z' {
        let t = [c];
        let e = KeyEvent { mods: C, utf8: &t, ..KeyEvent::default() };
        let want = match c {
            b'i' | b'm' => std::format!("\x1b[{c};5u").into_bytes(),
            _ => std::vec![c - b'a' + 1],
        };
        for platform in BOTH {
            assert_eq!(enc(&e, &EncodeOpts { platform, ..opts() }), want);
        }
    }
    is(text(Key::BracketLeft, C, "["), opts(), "\x1b[91;5u");
}

// "Legacy text": ctrl + C0 + alt gets the ESC prefix.
#[test]
fn inv10_ctrl_alt_esc_prefix() {
    is(text(Key::KeyA, C | A, "a"), opts(), "\x1b\x01");
    is(text(Key::Space, C | A, " "), opts(), "\x1b\x00");
}

// "Legacy text": ctrl + other char is `CSI cp;m u` (fixterms); shifted A–Z are lowercased.
#[test]
fn inv10_ctrl_other_char_csi_u() {
    is(text(Key::Quote, C, "'"), opts(), "\x1b[39;5u");
    is(text(Key::Unidentified, C, "é"), opts(), "\x1b[233;5u");
    let shifted = KeyEvent { unshifted_codepoint: Some('a'), ..text(Key::KeyA, C | S, "A") };
    is(shifted, opts(), "\x1b[97;6u");
}

// "Legacy text": alt + char is `ESC c` with ?1036; on macOS only when option is alt.
#[test]
fn inv10_alt_char() {
    let e = text(Key::KeyX, A, "x");
    let prefix = EncodeOpts { alt_esc_prefix: true, ..opts() };
    // Without 1036: the text.
    is(e, opts(), "x");
    assert_eq!(enc(&e, &prefix), b"\x1bx");
    let mac = EncodeOpts { platform: Platform::MacOs, ..prefix };
    assert_eq!(enc(&e, &mac), b"x");
    for (setting, side, want) in [
        (OptionAsAlt::True, Mods::NONE, "\x1bx"),
        (OptionAsAlt::Left, Mods::NONE, "\x1bx"),
        (OptionAsAlt::Left, Mods::ALT_RIGHT_SIDE, "x"),
        (OptionAsAlt::Right, Mods::ALT_RIGHT_SIDE, "\x1bx"),
        (OptionAsAlt::Right, Mods::NONE, "x"),
    ] {
        let o = EncodeOpts { macos_option_as_alt: setting, ..mac };
        let e = KeyEvent { mods: A | side, ..e };
        assert_eq!(enc(&e, &o), want.as_bytes(), "{setting:?} {side:?}");
    }
}

// "Legacy text": modifyOtherKeys 2 `CSI 27;m;cp~` for cp in 0x40–0x7F, any non-shift
// modifier, or shift+space.
#[test]
fn inv10_modify_other_keys_2() {
    let mok2 = EncodeOpts {
        modify_other_keys_state_2: true,
        macos_option_as_alt: OptionAsAlt::True,
        ..opts()
    };
    // 0x40–0x7F with shift only.
    is(text(Key::KeyA, S, "A"), mok2, "\x1b[27;2;65~");
    // A non-shift modifier on any code point.
    is(text(Key::Digit1, A, "1"), mok2, "\x1b[27;3;49~");
    // Shift + space.
    is(text(Key::Space, S, " "), mok2, "\x1b[27;2;32~");
    // Shift alone on another code point: the text.
    is(text(Key::Digit1, S, "!"), mok2, "!");
    // ctrl+a is still C0 (handled first).
    is(text(Key::KeyA, C, "a"), mok2, "\x01");
}

// "Legacy text": macOS Cmd + key sends nothing.
#[test]
fn inv10_macos_cmd_key_sends_nothing() {
    let e = text(Key::KeyC, U, "c");
    assert_eq!(enc(&e, &EncodeOpts::new(Platform::MacOs)), b"");
    assert_eq!(enc(&e, &EncodeOpts::new(Platform::Other)), b"c");
}

// "Legacy text": IME / composing: committed UTF-8 is sent; nothing while composing.
#[test]
fn inv10_ime_composing() {
    let committed = text(Key::Unidentified, Mods::NONE, "日本");
    is(committed, opts(), "日本");
    let composing = KeyEvent { composing: true, ..committed };
    is(composing, opts(), "");
    is(composing, kitty(KittyFlags::DISAMBIGUATE), "");
}

// "Kitty, flag 1": `CSI code;mods u`, `~`, or `CSI 1;m X`; the functional codes.
#[test]
fn inv10_kitty_disambiguate_functional_codes() {
    let o = kitty(KittyFlags::DISAMBIGUATE);
    for (key, want) in [
        (Key::Escape, "\x1b[27u"),
        (Key::Enter, "\x1b[13;5u"),
        (Key::Tab, "\x1b[9;5u"),
        (Key::Backspace, "\x1b[127;5u"),
        (Key::F13, "\x1b[57376;5u"),
        (Key::F25, "\x1b[57388;5u"),
        (Key::Numpad0, "\x1b[57399;5u"),
        (Key::NumpadBegin, "\x1b[57427;5u"),
        (Key::ScrollLock, "\x1b[57359;5u"),
        (Key::PrintScreen, "\x1b[57361;5u"),
        (Key::Pause, "\x1b[57362;5u"),
        (Key::Insert, "\x1b[2;5~"),
        (Key::F5, "\x1b[15;5~"),
        (Key::ArrowUp, "\x1b[1;5A"),
        (Key::F1, "\x1b[1;5P"),
    ] {
        let mods = if key == Key::Escape { Mods::NONE } else { C };
        is(k(key, mods), o, want);
    }
    // Caps lock, num lock and the modifier keys: 57358, 57360, 57441–57450, under report-all.
    let all = kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_ALL);
    for (key, code) in [
        (Key::CapsLock, 57358),
        (Key::NumLock, 57360),
        (Key::ShiftLeft, 57441),
        (Key::ControlLeft, 57442),
        (Key::AltLeft, 57443),
        (Key::MetaLeft, 57444),
        (Key::ShiftRight, 57447),
        (Key::ControlRight, 57448),
        (Key::AltRight, 57449),
        (Key::MetaRight, 57450),
    ] {
        is(k(key, Mods::NONE), all, &std::format!("\x1b[{code}u"));
        is(k(key, Mods::NONE), o, "");
    }
}

// "Kitty, no report-all": Enter/Tab/BS with no mods are legacy; unmodified text is text.
#[test]
fn inv10_kitty_no_report_all() {
    let o = kitty(KittyFlags::DISAMBIGUATE);
    is(k(Key::Enter, Mods::NONE), o, "\r");
    is(k(Key::Tab, Mods::NONE), o, "\t");
    is(k(Key::Backspace, Mods::NONE), o, "\x7f");
    is(text(Key::KeyQ, Mods::NONE, "q"), o, "q");
    // Shift consumed by the layout is not a modifier for this.
    let shifted = KeyEvent { consumed_mods: S, ..text(Key::KeyQ, S, "Q") };
    is(shifted, o, "Q");
}

// "Kitty, flag 2": `:2` repeat, `:3` release; press `:1` omitted (with mods 1, the field is
// omitted). In the `CSI 1;m X` form ghostty does send `:1` for a press (key_encode.zig:1068),
// which the inventory row's "never sent" misses.
#[test]
fn inv10_kitty_report_events() {
    let o = kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_EVENTS);
    let a = |action| KeyEvent { action, unshifted_codepoint: Some('a'), ..k(Key::KeyA, C) };
    is(a(Action::Press), o, "\x1b[97;5u");
    is(a(Action::Repeat), o, "\x1b[97;5:2u");
    is(a(Action::Release), o, "\x1b[97;5:3u");
    let unmodified = KeyEvent { action: Action::Repeat, ..k(Key::F13, Mods::NONE) };
    is(unmodified, o, "\x1b[57376;1:2u");
    is(k(Key::F13, Mods::NONE), o, "\x1b[57376u");
    is(k(Key::ArrowUp, Mods::NONE), o, "\x1b[1;1:1A");
    let up_release = KeyEvent { action: Action::Release, ..k(Key::ArrowUp, S) };
    is(up_release, o, "\x1b[1;2:3A");
    // Without flag 2, releases send nothing.
    is(a(Action::Release), kitty(KittyFlags::DISAMBIGUATE), "");
}

// "Kitty, flag 2 without flag 8": releases of Enter/Tab/BS are dropped.
#[test]
fn inv10_kitty_events_without_report_all() {
    let o = kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_EVENTS);
    for key in [Key::Enter, Key::Tab, Key::Backspace] {
        let rel = KeyEvent { action: Action::Release, ..k(key, C) };
        is(rel, o, "");
    }
    let rel = KeyEvent { action: Action::Release, ..k(Key::Escape, Mods::NONE) };
    is(rel, o, "\x1b[27;1:3u");
}

// "Kitty, flag 4": shifted and base-layout alternates, not for control codes.
#[test]
fn inv10_kitty_report_alternates() {
    let o = kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_ALTERNATES);
    let e = KeyEvent { unshifted_codepoint: Some('2'), ..text(Key::Digit2, S | C, "@") };
    is(e, o, "\x1b[50:64;6u");
    // Control codes (Escape 27 here) get none.
    is(text(Key::Escape, S, "\x1b"), o, "\x1b[27;2u");
}

// "Kitty, flag 8": everything as escapes, modifier-only keys too.
#[test]
fn inv10_kitty_report_all() {
    let o = kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_ALL);
    let a = KeyEvent { unshifted_codepoint: Some('a'), ..text(Key::KeyA, Mods::NONE, "a") };
    is(a, o, "\x1b[97u");
    is(k(Key::Enter, Mods::NONE), o, "\x1b[13u");
    is(k(Key::Tab, Mods::NONE), o, "\x1b[9u");
    is(k(Key::Backspace, Mods::NONE), o, "\x1b[127u");
    is(k(Key::ShiftLeft, S), o, "\x1b[57441;2u");
}

// "Kitty, flag 16": associated text, not with ctrl/super/alt (but macOS option), not on release.
#[test]
fn inv10_kitty_report_associated() {
    let o =
        kitty(KittyFlags::DISAMBIGUATE | KittyFlags::REPORT_ALL | KittyFlags::REPORT_ASSOCIATED);
    let e = |mods, t| KeyEvent { unshifted_codepoint: Some('a'), ..text(Key::KeyA, mods, t) };
    is(e(Mods::NONE, "a"), o, "\x1b[97;;97u");
    is(e(S, "A"), o, "\x1b[97;2;65u");
    is(e(C, "a"), o, "\x1b[97;5u");
    is(e(U, "a"), o, "\x1b[97;9u");
    // Alt prevents text on Linux; on macOS only when option is alt.
    let alt = e(A, "å");
    assert_eq!(enc(&alt, &o), b"\x1b[97;3u");
    let mac = EncodeOpts { platform: Platform::MacOs, ..o };
    assert_eq!(enc(&alt, &mac), b"\x1b[97;3;229u");
    let mac_alt = EncodeOpts { macos_option_as_alt: OptionAsAlt::True, ..mac };
    assert_eq!(enc(&alt, &mac_alt), b"\x1b[97;3u");
    // Not on release.
    let rel = KeyEvent { action: Action::Release, ..e(Mods::NONE, "a") };
    let ev = kitty(KittyFlags::ALL);
    is(rel, ev, "\x1b[97;1:3u");
    // Several code points.
    let multi = KeyEvent { unshifted_codepoint: Some('a'), ..text(Key::KeyA, Mods::NONE, "ab") };
    is(multi, o, "\x1b[97;;97:98u");
}

fn msize() -> Size {
    Size {
        screen: Pixels { width: 1000, height: 1000 },
        cell: Pixels { width: 1, height: 1 },
        padding: Padding::default(),
    }
}

fn mo(event: MouseEvents, format: MouseFormat) -> MouseOpts {
    MouseOpts { event, format, size: msize(), any_button_pressed: true }
}

fn me(button: Option<MouseButton>, action: MouseAction, mods: Mods, x: f32, y: f32) -> MouseEvent {
    MouseEvent { action, button, mods, pos: SurfacePos { x, y } }
}

fn menc(e: &MouseEvent, o: &MouseOpts) -> Vec<u8> {
    let mut buf = [0u8; mouse::MAX_LEN];
    mouse::encode(e, o, None, &mut buf).unwrap().to_vec()
}

const L: Option<MouseButton> = Some(MouseButton::Left);

// "Mouse X10 (format)": `CSI M` + (32+b), (33+x), (33+y); x, y ≤ 222, otherwise dropped.
#[test]
fn inv10_mouse_x10_format() {
    let o = mo(MouseEvents::Normal, MouseFormat::X10);
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 9.0, 4.0), &o), b"\x1b[M\x20\x2a\x25");
    assert_eq!(
        menc(&me(L, MouseAction::Press, Mods::NONE, 222.0, 222.0), &o),
        b"\x1b[M\x20\xff\xff"
    );
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 0.0, 223.0), &o), b"");
}

// "Mouse utf8 (1005)": `CSI M` + UTF-8(32+b, x+33, y+33).
#[test]
fn inv10_mouse_utf8() {
    let o = mo(MouseEvents::Normal, MouseFormat::Utf8);
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 9.0, 4.0), &o), b"\x1b[M\x20\x2a\x25");
    // 167 + 33 = U+00C8, two bytes; 0 + 33 = `!`.
    assert_eq!(
        menc(&me(L, MouseAction::Press, Mods::NONE, 167.0, 0.0), &o),
        b"\x1b[M\x20\xc3\x88!"
    );
}

// "Mouse SGR (1006)": `CSI < b;x;y M` / `m` on release.
#[test]
fn inv10_mouse_sgr() {
    let o = mo(MouseEvents::Normal, MouseFormat::Sgr);
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 9.0, 4.0), &o), b"\x1b[<0;10;5M");
    assert_eq!(menc(&me(L, MouseAction::Release, Mods::NONE, 9.0, 4.0), &o), b"\x1b[<0;10;5m");
    assert_eq!(
        menc(&me(L, MouseAction::Press, Mods::NONE, 999.0, 999.0), &o),
        b"\x1b[<0;1000;1000M"
    );
}

// "Mouse urxvt (1015)": `CSI 32+b;x;y M`.
#[test]
fn inv10_mouse_urxvt() {
    let o = mo(MouseEvents::Normal, MouseFormat::Urxvt);
    assert_eq!(
        menc(&me(Some(MouseButton::Middle), MouseAction::Press, Mods::NONE, 9.0, 4.0), &o),
        b"\x1b[33;10;5M"
    );
    assert_eq!(
        menc(&me(Some(MouseButton::Middle), MouseAction::Release, Mods::NONE, 9.0, 4.0), &o),
        b"\x1b[35;10;5M"
    );
}

// "Mouse SGR-pixels (1016)": SGR with pixel coordinates, rounded, may be negative or outside.
#[test]
fn inv10_mouse_sgr_pixels() {
    let mut o = mo(MouseEvents::Any, MouseFormat::SgrPixels);
    o.size.padding = Padding { top: 10, bottom: 0, right: 0, left: 10 };
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 15.6, 12.4), &o), b"\x1b[<0;6;2M");
    // Above-left of the grid: negative.
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, 2.0, 3.0), &o), b"\x1b[<0;-8;-7M");
    // Outside the surface while dragging.
    assert_eq!(
        menc(&me(L, MouseAction::Motion, Mods::NONE, 1500.0, -20.0), &o),
        b"\x1b[<32;1490;-30M"
    );
}

// "Mouse button codes": L 0, M 1, R 2, wheel 4–7 → 64–67, 8/9 → 128/129; shift +4, alt +8,
// ctrl +16, motion +32; no-button motion 3; legacy release 3; X10 mode no modifiers and only
// L/M/R presses.
#[test]
fn inv10_mouse_button_codes() {
    let o = mo(MouseEvents::Any, MouseFormat::Sgr);
    for (b, code) in [
        (MouseButton::Left, 0),
        (MouseButton::Middle, 1),
        (MouseButton::Right, 2),
        (MouseButton::Four, 64),
        (MouseButton::Five, 65),
        (MouseButton::Six, 66),
        (MouseButton::Seven, 67),
        (MouseButton::Eight, 128),
        (MouseButton::Nine, 129),
    ] {
        assert_eq!(
            menc(&me(Some(b), MouseAction::Press, Mods::NONE, 0.0, 0.0), &o),
            std::format!("\x1b[<{code};1;1M").as_bytes()
        );
    }
    for (m, add) in [(S, 4), (A, 8), (C, 16), (S | A | C, 28)] {
        assert_eq!(
            menc(&me(L, MouseAction::Press, m, 0.0, 0.0), &o),
            std::format!("\x1b[<{add};1;1M").as_bytes()
        );
    }
    assert_eq!(menc(&me(L, MouseAction::Motion, S, 0.0, 0.0), &o), b"\x1b[<36;1;1M");
    assert_eq!(menc(&me(None, MouseAction::Motion, Mods::NONE, 0.0, 0.0), &o), b"\x1b[<35;1;1M");
    let legacy = mo(MouseEvents::Any, MouseFormat::X10);
    assert_eq!(
        menc(&me(Some(MouseButton::Right), MouseAction::Release, Mods::NONE, 0.0, 0.0), &legacy),
        b"\x1b[M\x23!!"
    );
    let x10 = mo(MouseEvents::X10, MouseFormat::Sgr);
    assert_eq!(
        menc(&me(Some(MouseButton::Right), MouseAction::Press, C | S, 0.0, 0.0), &x10),
        b"\x1b[<2;1;1M"
    );
    assert_eq!(
        menc(&me(Some(MouseButton::Four), MouseAction::Press, Mods::NONE, 0.0, 0.0), &x10),
        b""
    );
}

// "Mouse event modes": 1000 no motion, 1002 motion with a button held, 1003 all; motion only on
// a cell change (but SGR-pixels); outside the viewport release always, motion only in motion
// modes with a button held.
#[test]
fn inv10_mouse_event_modes() {
    let motion = me(L, MouseAction::Motion, Mods::NONE, 3.0, 3.0);
    let hover = me(None, MouseAction::Motion, Mods::NONE, 3.0, 3.0);
    assert_eq!(menc(&motion, &mo(MouseEvents::Normal, MouseFormat::Sgr)), b"");
    assert_eq!(menc(&motion, &mo(MouseEvents::Button, MouseFormat::Sgr)), b"\x1b[<32;4;4M");
    assert_eq!(menc(&hover, &mo(MouseEvents::Button, MouseFormat::Sgr)), b"");
    assert_eq!(menc(&hover, &mo(MouseEvents::Any, MouseFormat::Sgr)), b"\x1b[<35;4;4M");
    assert_eq!(menc(&motion, &mo(MouseEvents::None, MouseFormat::Sgr)), b"");

    // Cell-change dedup.
    let o = mo(MouseEvents::Any, MouseFormat::Sgr);
    let mut last: Option<Cell> = None;
    let mut buf = [0u8; mouse::MAX_LEN];
    let step = |x: f32, last: &mut Option<Cell>, buf: &mut [u8; mouse::MAX_LEN], o: &MouseOpts| {
        mouse::encode(&me(L, MouseAction::Motion, Mods::NONE, x, 3.0), o, Some(last), buf)
            .unwrap()
            .to_vec()
    };
    assert_eq!(step(3.2, &mut last, &mut buf, &o), b"\x1b[<32;4;4M");
    assert_eq!(step(3.7, &mut last, &mut buf, &o), b"");
    assert_eq!(step(4.1, &mut last, &mut buf, &o), b"\x1b[<32;5;4M");
    let px = mo(MouseEvents::Any, MouseFormat::SgrPixels);
    assert_eq!(step(4.1, &mut last, &mut buf, &px), b"\x1b[<32;4;3M");

    // Outside the viewport.
    let mut out = mo(MouseEvents::Normal, MouseFormat::Sgr);
    assert_eq!(
        menc(&me(L, MouseAction::Release, Mods::NONE, -5.0, 2000.0), &out),
        b"\x1b[<0;1;1000m"
    );
    assert_eq!(menc(&me(L, MouseAction::Press, Mods::NONE, -5.0, 2.0), &out), b"");
    out.event = MouseEvents::Button;
    assert_eq!(menc(&me(L, MouseAction::Motion, Mods::NONE, -5.0, 2.0), &out), b"\x1b[<32;1;3M");
    out.any_button_pressed = false;
    assert_eq!(menc(&me(L, MouseAction::Motion, Mods::NONE, -5.0, 2.0), &out), b"");
}

// "Focus (1004)": `CSI I` / `CSI O`.
#[test]
fn inv10_focus() {
    let on = FocusOpts { reporting: true };
    assert_eq!(focus::encode(FocusEvent::Gained, on), b"\x1b[I");
    assert_eq!(focus::encode(FocusEvent::Lost, on), b"\x1b[O");
    assert_eq!(focus::encode(FocusEvent::Gained, FocusOpts::default()), b"");
}

// "Paste": NUL, BS, ENQ, EOT, ESC, DEL, ^C, ^\, ^U, ^Z, ^Q, ^S, ^W, ^V, ^R, ^O become spaces.
#[test]
fn inv10_paste_unsafe_bytes() {
    let data = *b"\x00\x08\x05\x04\x1b\x7f\x03\x1c\x15\x1a\x11\x13\x17\x16\x12\x0f\x01\x07\t";
    for bracketed in [false, true] {
        let mut d = data;
        let r = paste::encode(&mut d, PasteOpts { bracketed });
        assert_eq!(r.data, b"                \x01\x07\t");
    }
}

// "Paste, bracketed (2004)": wrapped in `CSI 200~` … `CSI 201~`.
#[test]
fn inv10_paste_bracketed() {
    let mut data = *b"a\nb";
    let r = paste::encode(&mut data, PasteOpts { bracketed: true });
    assert_eq!([r.prefix, r.data, r.suffix].concat(), b"\x1b[200~a\nb\x1b[201~");
}

// "Paste, not bracketed": LF to CR; `is_safe` rejects `\n` or `ESC[201~`.
#[test]
fn inv10_paste_not_bracketed_and_is_safe() {
    let mut data = *b"a\nb\r\n";
    let r = paste::encode(&mut data, PasteOpts { bracketed: false });
    assert_eq!([r.prefix, r.data, r.suffix].concat(), b"a\rb\r\r");
    assert!(paste::is_safe(b"echo hi"));
    assert!(!paste::is_safe(b"echo hi\n"));
    assert!(!paste::is_safe(b"x\x1b[201~y"));
    assert!(paste::is_safe(b"x\x1b[200~y"));
}
