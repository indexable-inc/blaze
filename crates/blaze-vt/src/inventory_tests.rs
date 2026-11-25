//! One test per row of vt-inventory.md §2 (C0/C1), §3 (ESC), §4 (CSI but SGR), §5 (modes) and
//! §11 (screen model) that terminal_tests.rs does not already cover. Each names its row.

use std::vec::Vec as StdVec;

use crate::page::RowPrompt;
use crate::screen::{Charset, CursorShape, ProtectedMode};
use crate::terminal::{ColorScheme, Effect, Mode, StatusDisplay, Terminal, Which};
use crate::test_support::{cell, cursor, fed, heap, history_text, opts, replies, term, text};

fn unsupported<P: blaze_own::planted::Planted<crate::plant::VtPlant>>(t: &Terminal<'_, P>) -> u64 {
    t.stats().0.unsupported
}

fn effects(t: &mut Terminal<'_>) -> StdVec<Effect> {
    t.take_effects().as_slice().to_vec()
}

// ---- §2 C0 / C1 ---------------------------------------------------------------------------

#[test]
fn c0_ignored_and_unsupported() {
    // NUL, SOH, STX ignored; ETX, EOT, ACK logged as unsupported (S:1057).
    let t = fed(10, 2, b"\x00\x01\x02A");
    assert_eq!((text(&t), unsupported(&t)), ("A".into(), 0));
    let t = fed(10, 2, b"\x03\x04\x06A");
    assert_eq!((text(&t), unsupported(&t)), ("A".into(), 3));
}

#[test]
fn c0_enq_replies_with_the_configured_answer() {
    let mut o = opts(10, 2);
    o.enquiry = b"blaze";
    let mut t = Terminal::new_in(o, heap()).unwrap();
    t.feed(b"\x05");
    assert_eq!(replies(&mut t), b"blaze");
}

#[test]
fn c0_bel_bs_ht_cr() {
    let mut t = fed(20, 2, b"\x07");
    assert_eq!(effects(&mut t), [Effect::Bell]);
    assert_eq!(cursor(&fed(20, 2, b"AB\x08")), (1, 0));
    assert_eq!(cursor(&fed(20, 2, b"A\t")), (8, 0));
    assert_eq!(cursor(&fed(20, 2, b"AB\r")), (0, 0));
}

#[test]
fn c0_lf_vt_ff_are_linefeeds() {
    for b in [b'\n', 0x0B, 0x0C] {
        let t = fed(10, 3, &[b'A', b, b'B']);
        assert_eq!(text(&t), "A\n B", "byte {b:#x}");
    }
    // With LNM, each also returns the carriage.
    let t = fed(10, 3, b"\x1b[20hA\x0bB");
    assert_eq!(text(&t), "A\nB");
}

#[test]
fn c0_so_si_shift_g1_and_g0() {
    let t = fed(10, 2, b"\x1b)0\x0eq\x0fq");
    assert_eq!(text(&t), "\u{2500}q");
}

#[test]
fn c0_above_0x0f_and_del_print_in_ground() {
    // The quirk: 0x10-0x17, 0x19, 0x1C-0x1F, CAN/SUB in ground and DEL print as cells.
    for b in [0x10u8, 0x17, 0x18, 0x19, 0x1A, 0x1C, 0x1F, 0x7F] {
        let t = fed(10, 2, &[b]);
        assert_eq!(cell(&t, 0, 0).codepoint(), u32::from(b), "byte {b:#x}");
        assert_eq!(cursor(&t), (1, 0));
    }
}

#[test]
fn can_and_sub_cancel_a_sequence_del_is_ignored_in_one() {
    assert_eq!(cursor(&fed(10, 2, b"\x1b[5\x18A")), (1, 0));
    assert_eq!(cursor(&fed(10, 2, b"\x1b[5\x1aA")), (1, 0));
    assert_eq!(cursor(&fed(10, 2, b"\x1b[3\x7fC")), (3, 0));
}

#[test]
fn c1_controls_mid_sequence_act_as_esc_fe() {
    // C1 bytes only work mid-sequence (in ground they are invalid UTF-8).
    assert_eq!(cursor(&fed(10, 3, b"A\x1b[\x84")), (1, 1), "IND");
    assert_eq!(cursor(&fed(10, 3, b"A\x1b[\x85")), (0, 1), "NEL");
    assert_eq!(cursor(&fed(20, 3, b"\x1b[3g\x1b[1;4H\x1b[\x88\r\t")), (3, 0), "HTS");
    assert_eq!(text(&fed(10, 2, b"A\x1b[\x8dB")), " B\nA", "RI");
    assert_eq!(text(&fed(10, 2, b"\x1b*0\x1b[\x8e``")), "\u{25C6}`", "SS2");
    assert_eq!(text(&fed(10, 2, b"\x1b+0\x1b[\x8f``")), "\u{25C6}`", "SS3");
    let t = fed(10, 2, b"\x1b[\x96A\x1b[\x97B\x1b[1;1H\x1b[K");
    assert_eq!(text(&t), "A", "SPA / EPA");
    let mut t = fed(10, 2, b"\x1b[\x9a");
    assert_eq!(replies(&mut t), b"\x1b[?62;22c", "DECID");
    let t = fed(10, 2, b"\x1b[\x9b3CX");
    assert_eq!(text(&t), "   X", "CSI");
    let t = fed(10, 2, b"\x1b[\x9cX");
    assert_eq!(text(&t), "X", "ST goes to ground");
    let t = fed(10, 2, b"\x1b[\x80");
    assert_eq!(unsupported(&t), 1, "other C1: ESC Fe with no handler");
}

#[test]
fn c1_dcs_osc_sos_pm_apc_open_strings() {
    // Each opens its string; the stubs swallow it and the text after ST prints.
    for opener in [0x90u8, 0x9D, 0x98, 0x9E, 0x9F] {
        let mut bytes = std::vec![0x1b, b'[', opener];
        bytes.extend_from_slice(b"x\x1b\\Y");
        let t = fed(10, 2, &bytes);
        assert_eq!(text(&t), "Y", "opener {opener:#x}");
    }
}

#[test]
fn utf8_c1_codepoints_print_in_ground() {
    let t = fed(10, 2, "\u{85}\u{9b}".as_bytes());
    assert_eq!(cell(&t, 0, 0).codepoint(), 0x85);
    assert_eq!(cell(&t, 1, 0).codepoint(), 0x9b);
}

// ---- §3 ESC -------------------------------------------------------------------------------

#[test]
fn esc_scs_designates_each_slot() {
    let t = fed(10, 2, b"\x1b(0\x1b)A\x1b*B\x1b+0");
    let sets = t.screen().charset.sets;
    assert_eq!(sets, [Charset::DecSpecial, Charset::British, Charset::Ascii, Charset::DecSpecial]);
    // 96-character sets (- . /) are not supported.
    let t = fed(10, 2, b"\x1b-A\x1b.A\x1b/A");
    assert_eq!(unsupported(&t), 3);
}

#[test]
fn esc_nel() {
    let t = fed(10, 3, b"AB\x1bEC");
    assert_eq!((text(&t), cursor(&t)), ("AB\nC".into(), (1, 1)));
}

#[test]
fn esc_decid_is_da1() {
    let mut t = fed(10, 2, b"\x1bZ");
    assert_eq!(replies(&mut t), b"\x1b[?62;22c");
}

#[test]
fn esc_ls2_ls3_and_gr_shifts() {
    let t = fed(10, 2, b"\x1b*0\x1bn`");
    assert_eq!(text(&t), "\u{25C6}");
    let t = fed(10, 2, b"\x1b+0\x1bo`");
    assert_eq!(text(&t), "\u{25C6}");
    for (seq, slot) in [
        (b"\x1b~", crate::screen::Slot::G1),
        (b"\x1b}", crate::screen::Slot::G2),
        (b"\x1b|", crate::screen::Slot::G3),
    ] {
        let t = fed(10, 2, seq);
        assert_eq!(t.screen().charset.gr, slot);
    }
}

#[test]
fn esc_deckpam_deckpnm() {
    let mut t = fed(10, 2, b"\x1b=");
    assert!(t.mode(Mode::KeypadKeys));
    t.feed(b"\x1b>");
    assert!(!t.mode(Mode::KeypadKeys));
}

#[test]
fn esc_st_is_a_no_op_and_others_are_unsupported() {
    let t = fed(10, 2, b"\x1b\\");
    assert_eq!(unsupported(&t), 0);
    let t = fed(10, 2, b"\x1b#3\x1b#4\x1b#5\x1b#6\x1b%G\x1b%@\x1b6\x1b9\x1b F\x1b G");
    assert_eq!(unsupported(&t), 10);
}

#[test]
fn esc_decaln_resets_margins_origin_and_keeps_fg_bg() {
    // T:3277.
    let t = fed(5, 3, b"\x1b[2;3r\x1b[?6h\x1b[1;41m\x1b#8");
    assert_eq!(text(&t), "EEEEE\nEEEEE\nEEEEE");
    assert_eq!((t.region().top, t.region().bottom), (0, 2));
    assert!(!t.mode(Mode::Origin));
    assert_eq!(cursor(&t), (0, 0));
    let st = t.screen().style(cell(&t, 0, 0).style_id());
    assert!(!st.flags.bold, "only fg and bg are kept");
    assert_ne!(st.bg, crate::style::Color::None);
}

#[test]
fn esc_decrc_with_nothing_saved_goes_home() {
    let t = fed(10, 5, b"\x1b[3;3H\x1b8");
    assert_eq!(cursor(&t), (0, 0));
}

// ---- §4 CSI -------------------------------------------------------------------------------

#[test]
fn csi_cursor_up_down_clamp_to_the_region() {
    // CUU / CUD clamp to the margin when inside the region (T:1502, 1520).
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;6r\x1b[5;1H\x1b[10A")), (0, 2));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;6r\x1b[1;1H\x1b[10A")), (0, 0));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;6r\x1b[4;1H\x1b[10B")), (0, 5));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;6r\x1b[8;1H\x1b[10B")), (0, 9));
    // VPB is CUU.
    assert_eq!(cursor(&fed(10, 10, b"\x1b[5;1H\x1b[2k")), (0, 2));
    // 0 is 1.
    assert_eq!(cursor(&fed(10, 10, b"\x1b[5;1H\x1b[0A")), (0, 3));
}

#[test]
fn csi_cursor_right_left_clamp_to_the_margins() {
    assert_eq!(cursor(&fed(10, 2, b"\x1b[?69h\x1b[1;5s\x1b[1;2H\x1b[10C")), (4, 0));
    assert_eq!(cursor(&fed(10, 2, b"\x1b[10C")), (9, 0));
    assert_eq!(cursor(&fed(10, 2, b"\x1b[1;8H\x1b[3D")), (4, 0));
    // HPB is CUB.
    assert_eq!(cursor(&fed(10, 2, b"\x1b[1;8H\x1b[3j")), (4, 0));
}

#[test]
fn csi_cub_with_pending_wrap_counts_one_less() {
    // T:1551: under reverse wrap a pending wrap takes one from the count.
    let t = fed(5, 2, b"\x1b[?45hABCDE\x1b[DX");
    assert_eq!(text(&t), "ABCDX");
}

#[test]
fn csi_cnl_cpl() {
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[2E")), (0, 4));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[2F")), (0, 0));
}

#[test]
fn csi_cha_hpa_vpa() {
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[7G")), (6, 2));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[7`")), (6, 2));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[7d")), (3, 6));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[2a")), (5, 2));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[3;4H\x1b[2e")), (3, 4));
}

#[test]
fn csi_cup_hvp_and_three_params_is_invalid() {
    assert_eq!(cursor(&fed(10, 10, b"\x1b[2;3f")), (2, 1));
    assert_eq!(cursor(&fed(10, 10, b"\x1b[;3H")), (2, 0));
    let t = fed(10, 10, b"\x1b[2;2H\x1b[3;3;3H");
    assert_eq!((cursor(&t), unsupported(&t)), ((1, 1), 1));
}

#[test]
fn csi_ed_modes() {
    // ED 3: scrollback erased (T:3270).
    let t = fed(5, 2, b"1\r\n2\r\n3\x1b[3J");
    assert_eq!((history_text(&t), text(&t)), ("".into(), "2\n3".into()));
    // ED 22: the screen goes into scrollback (T:3156).
    let t = fed(5, 3, b"1\r\n2\x1b[22J");
    assert_eq!((history_text(&t), text(&t)), ("1\n2".into(), "".into()));
    // Anything else is invalid.
    let t = fed(5, 3, b"A\x1b[4J");
    assert_eq!((text(&t), unsupported(&t)), ("A".into(), 1));
}

#[test]
fn csi_ed2_at_a_prompt_scrolls_into_history() {
    // T:3176, ghostty #905: ^L at a prompt keeps the screen in scrollback.
    let mut t = fed(5, 3, b"1\r\n2\r\n$ ");
    let s = t.screen_mut();
    let r = s.r(2);
    s.pages.row_mut(r).prompt = RowPrompt::Prompt;
    t.feed(b"\x1b[2J");
    assert_eq!(history_text(&t), "1\n2\n$ ");
    assert_eq!(text(&t), "");
    // Not at a prompt: nothing kept.
    let t = fed(5, 3, b"1\r\n2\r\n$ \x1b[2J");
    assert_eq!(history_text(&t), "");
    // The alternate screen never keeps it.
    let mut t = fed(5, 3, b"\x1b[?1049h1\r\n2");
    let s = t.screen_mut();
    let r = s.r(2);
    s.pages.row_mut(r).prompt = RowPrompt::Prompt;
    t.feed(b"\x1b[2J");
    assert_eq!(history_text(&t), "");
}

#[test]
fn csi_el_mode_4_and_beyond_are_invalid() {
    let t = fed(5, 3, b"ABC\x1b[1;1H\x1b[4K");
    assert_eq!((text(&t), unsupported(&t)), ("ABC".into(), 1));
}

#[test]
fn csi_el_clears_pending_wrap() {
    for mode in [b'0', b'1', b'2'] {
        let t = fed(5, 3, &[b'A', b'B', b'C', b'D', b'E', 0x1b, b'[', mode, b'K']);
        assert!(!t.screen().cursor.pending_wrap, "EL {}", mode as char);
    }
}

#[test]
fn csi_dl_zero_returns_early_and_il_dl_go_to_the_left_margin() {
    let t = fed(5, 3, b"ABC\r\nDEF\x1b[1;2H\x1b[0M");
    assert_eq!((text(&t), cursor(&t)), ("ABC\nDEF".into(), (1, 0)));
    let t = fed(10, 3, b"\x1b[?69h\x1b[3;6s\x1b[1;5H\x1b[M");
    assert_eq!(cursor(&t), (2, 0));
}

#[test]
fn csi_dch_outside_the_margins_is_a_no_op() {
    let t = fed(5, 3, b"ABCDE\x1b[?69h\x1b[2;4s\x1b[1;5H\x1b[P");
    assert_eq!(text(&t), "ABCDE");
}

#[test]
fn csi_su_sd_count_clamped_to_the_region() {
    let t = fed(5, 3, b"1\r\n2\r\n3\x1b[2;3r\x1b[99S");
    assert_eq!(text(&t), "1");
    let t = fed(5, 3, b"1\r\n2\r\n3\x1b[2;3r\x1b[99T");
    assert_eq!(text(&t), "1");
}

#[test]
fn csi_ctc_invalid_values() {
    let t = fed(20, 2, b"\x1b[1;5H\x1b[3W\x1b[1W");
    assert_eq!(unsupported(&t), 2);
    let t = fed(20, 2, b"\x1b[1;9H\x1b[2W");
    assert!(!t.is_tabstop(8));
}

#[test]
fn csi_cbt_is_bounded_by_the_left_margin_only_in_origin_mode() {
    // T:1923.
    assert_eq!(cursor(&fed(20, 2, b"\x1b[?69h\x1b[3;10s\x1b[1;5H\x1b[Z")), (0, 0));
    assert_eq!(cursor(&fed(20, 2, b"\x1b[?69h\x1b[3;10s\x1b[?6h\x1b[1;3H\x1b[Z")), (2, 0));
}

#[test]
fn csi_xtmodkeys_forms() {
    let mut t = term(10, 2);
    t.feed(b"\x1b[>4;2m");
    assert!(t.flags().modify_other_keys_2);
    t.feed(b"\x1b[>m");
    assert!(!t.flags().modify_other_keys_2, "no params: legacy");
    t.feed(b"\x1b[>4;2m\x1b[>4;2;1m");
    assert!(t.flags().modify_other_keys_2, "three params are invalid");
    assert_eq!(unsupported(&t), 1);
    t.feed(b"\x1b[>4n");
    assert!(!t.flags().modify_other_keys_2, "CSI > n resets");
}

#[test]
fn csi_xtqmodkeys_and_decstr_are_ignored() {
    let t = fed(10, 2, b"\x1b[?4m\x1b[!p");
    assert_eq!(unsupported(&t), 2);
}

#[test]
fn csi_dsr_needs_one_param() {
    let mut t = fed(10, 2, b"\x1b[n\x1b[5;1n");
    assert_eq!(replies(&mut t), b"");
}

#[test]
fn csi_decscusr_all_values() {
    let cases = [
        // 0 is the default: stream_terminal.zig:188 default_cursor_blink = false.
        (0, CursorShape::Block, false),
        (1, CursorShape::Block, true),
        (2, CursorShape::Block, false),
        (3, CursorShape::Underline, true),
        (4, CursorShape::Underline, false),
        (5, CursorShape::Bar, true),
        (6, CursorShape::Bar, false),
    ];
    for (n, shape, blink) in cases {
        let t = fed(10, 2, std::format!("\x1b[{n} q").as_bytes());
        assert_eq!((t.screen().cursor.shape, t.mode(Mode::CursorBlinking)), (shape, blink), "{n}");
    }
}

#[test]
fn csi_decsca_values() {
    let t = fed(10, 2, b"\x1b[1\"q");
    assert_eq!(t.screen().protected_mode, ProtectedMode::Dec);
    assert!(t.screen().cursor.protected);
    let t = fed(10, 2, b"\x1b[1\"q\x1b[2\"q");
    assert!(!t.screen().cursor.protected);
    let t = fed(10, 2, b"\x1b[1;1\"q");
    assert_eq!(unsupported(&t), 1);
}

#[test]
fn csi_xtversion_takes_any_params() {
    let mut o = opts(10, 2);
    o.xtversion = b"blaze 1";
    let mut t = Terminal::new_in(o, heap()).unwrap();
    t.feed(b"\x1b[>0q\x1b[>1;2q");
    assert_eq!(replies(&mut t), b"\x1bP>|blaze 1\x1b\\\x1bP>|blaze 1\x1b\\");
}

#[test]
fn csi_decstbm_bottom_zero_and_homing() {
    let t = fed(10, 10, b"\x1b[5;5H\x1b[3;0r");
    assert_eq!((t.region().top, t.region().bottom, cursor(&t)), (2, 9, (0, 0)));
    let t = fed(10, 10, b"\x1b[3;6r\x1b[?6h\x1b[5;5H\x1b[4;8r");
    assert_eq!(cursor(&t), (0, 3), "home is the region's top in origin mode");
}

#[test]
fn csi_decslrm_invalid_is_a_no_op() {
    let t = fed(10, 10, b"\x1b[?69h\x1b[5;5H\x1b[6;3s");
    assert_eq!((t.region().left, t.region().right, cursor(&t)), (0, 9, (4, 4)));
}

#[test]
fn csi_xtsave_xtrestore_one_slot_per_mode() {
    let t = fed(10, 2, b"\x1b[?7s\x1b[?7l\x1b[?7s\x1b[?7h\x1b[?7r");
    assert!(!t.mode(Mode::Wraparound), "the second save replaced the first");
    // Restore with nothing saved restores the default... as ghostty: the saved bit starts off.
    let t = fed(10, 2, b"\x1b[?45h\x1b[?45r");
    assert!(!t.mode(Mode::ReverseWrap));
}

#[test]
fn csi_xtshiftescape() {
    let mut t = term(10, 2);
    t.feed(b"\x1b[>0s");
    assert_eq!(t.flags().mouse_shift_capture, Some(false));
    t.feed(b"\x1b[>1s");
    assert_eq!(t.flags().mouse_shift_capture, Some(true));
    t.feed(b"\x1b[>2s");
    assert_eq!(t.flags().mouse_shift_capture, Some(true), "2 is invalid");
}

#[test]
fn csi_xtwinops_other_forms_are_unsupported() {
    let mut t = fed(10, 2, b"\x1b[t\x1b[9t\x1b[18;1t");
    assert_eq!(replies(&mut t), b"");
    assert_eq!(unsupported(&t), 3);
}

#[test]
fn csi_title_push_pop_with_index() {
    // 22;Ps;idx and 23;Ps;idx: the index is accepted and ignored.
    let mut t = term(10, 2);
    t.set_title(b"a");
    t.feed(b"\x1b[22;0;5t");
    t.set_title(b"b");
    t.feed(b"\x1b[23;0;5t");
    assert_eq!(t.title(), b"a");
}

#[test]
fn csi_scorc() {
    assert_eq!(cursor(&fed(10, 5, b"\x1b[3;3H\x1b[s\x1b[H\x1b[u")), (2, 2));
}

#[test]
fn csi_kitty_pop_default_and_reset() {
    let mut t = fed(10, 2, b"\x1b[>1u\x1b[>2u\x1b[<u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?1u");
    t.feed(b"\x1b[>1u\x1b[>2u\x1b[<99u\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?0u");
}

#[test]
fn csi_ich_clears_pending_wrap_outside_the_margins() {
    let t = fed(5, 2, b"\x1b[?69h\x1b[1;3s\x1b[1;5HX\x1b[@");
    assert!(!t.screen().cursor.pending_wrap);
    assert_eq!(text(&t), "    X");
}

#[test]
fn csi_decsasd() {
    let t = fed(10, 2, b"\x1b[1$}");
    assert_eq!(t.status_display(), StatusDisplay::StatusLine);
    let t = fed(10, 2, b"\x1b[1$}\x1b[0$}");
    assert_eq!(t.status_display(), StatusDisplay::Main);
    // Exactly one param (S:2238): a bare one is ignored.
    let t = fed(10, 2, b"\x1b[1$}\x1b[$}");
    assert_eq!((t.status_display(), unsupported(&t)), (StatusDisplay::StatusLine, 1));
}

#[test]
fn csi_sm_rm_loop_over_params() {
    let t = fed(10, 2, b"\x1b[?45;1045;9999h");
    assert!(t.mode(Mode::ReverseWrap) && t.mode(Mode::ReverseWrapExtended));
    assert_eq!(unsupported(&t), 1, "the unknown mode is logged");
    let t = fed(10, 2, b"\x1b[4;20h\x1b[4;20l");
    assert!(!t.mode(Mode::Insert) && !t.mode(Mode::Linefeed));
}

// ---- §5 modes -----------------------------------------------------------------------------

#[test]
fn mode_irm_not_at_the_end_of_the_line() {
    // T:1173: insert only when x + width < cols.
    let t = fed(5, 2, b"ABCDE\x1b[1;5H\x1b[4hX");
    assert_eq!(text(&t), "ABCDX");
}

#[test]
fn mode_decscnm_is_an_effect() {
    let mut t = fed(10, 2, b"\x1b[?5h\x1b[?5l");
    assert_eq!(effects(&mut t), [Effect::ReverseColors(true), Effect::ReverseColors(false)]);
}

#[test]
fn mode_decom_homes_the_cursor() {
    let t = fed(10, 10, b"\x1b[3;6r\x1b[?69h\x1b[4;8s\x1b[5;5H\x1b[?6h");
    assert_eq!(cursor(&t), (3, 2));
    let t = fed(10, 10, b"\x1b[3;6r\x1b[?6h\x1b[5;5H\x1b[?6l");
    assert_eq!(cursor(&t), (0, 0));
}

#[test]
fn mode_40_is_an_effect() {
    let mut t = fed(10, 2, b"\x1b[?40h\x1b[?40l");
    assert_eq!(effects(&mut t), [Effect::EnableMode3(true), Effect::EnableMode3(false)]);
}

#[test]
fn mode_deccolm_back_to_80() {
    let mut t = fed(132, 5, b"\x1b[?40h\x1b[?3h");
    assert_eq!(t.cols(), 132);
    t.feed(b"\x1b[?3l");
    assert_eq!(t.cols(), 80);
    assert!(effects(&mut t).contains(&Effect::ColumnMode { cols: 80 }));
}

#[test]
fn mode_2031_reports_on_set() {
    let mut t = term(10, 2);
    t.set_color_scheme(Some(ColorScheme::Dark));
    t.feed(b"\x1b[?2031h");
    assert_eq!(replies(&mut t), b"\x1b[?997;1n");
}

#[test]
fn mode_mouse_off_resets_event_and_format() {
    use crate::terminal::{MouseEvents, MouseFormat};
    for (on, off, ev) in [
        (9, 9, MouseEvents::X10),
        (1000, 1003, MouseEvents::Normal),
        (1003, 1002, MouseEvents::Any),
    ] {
        let mut t = fed(10, 2, std::format!("\x1b[?{on}h").as_bytes());
        assert_eq!(t.flags().mouse_event, ev);
        t.feed(std::format!("\x1b[?{off}l").as_bytes());
        assert_eq!(t.flags().mouse_event, MouseEvents::None);
    }
    for (n, f) in [
        (1005, MouseFormat::Utf8),
        (1006, MouseFormat::Sgr),
        (1015, MouseFormat::Urxvt),
        (1016, MouseFormat::SgrPixels),
    ] {
        let mut t = fed(10, 2, std::format!("\x1b[?{n}h").as_bytes());
        assert_eq!(t.flags().mouse_format, f);
        t.feed(b"\x1b[?1005l");
        assert_eq!(t.flags().mouse_format, MouseFormat::X10);
    }
}

#[test]
fn mode_1004_reports_focus_state() {
    let mut t = term(10, 2);
    t.set_focused(false);
    t.feed(b"\x1b[?1004h");
    assert_eq!(replies(&mut t), b"\x1b[O");
}

#[test]
fn mode_defaults_from_options() {
    let mut o = opts(10, 2);
    o.grapheme_cluster = true;
    let t = Terminal::new_in(o, heap()).unwrap();
    assert!(t.mode(Mode::GraphemeCluster));
}

// ---- §11 screen model ---------------------------------------------------------------------

#[test]
fn screen_wrap_flag_only_at_the_real_last_column() {
    // T:882 / 1410: a wrap at the right margin does not mark the row.
    let t = fed(10, 5, b"\x1b[?69h\x1b[1;5s\x1b[1;1HABCDEF");
    assert!(!t.screen().pages.row(t.screen().r(0)).wrap);
    assert_eq!(text(&t), "ABCDE\nF");
}

#[test]
fn screen_cell_hyperlink_is_stored() {
    let mut t = term(10, 2);
    t.screen_mut().start_hyperlink(b"https://example.com", None);
    t.feed(b"A");
    t.screen_mut().end_hyperlink();
    t.feed(b"B");
    let s = t.screen();
    assert_ne!(s.cell_link(cell(&t, 0, 0)), 0);
    assert_eq!(s.cell_link(cell(&t, 1, 0)), 0);
    assert_eq!(s.links.live(), 1);
}

#[test]
fn screen_ri_scrolls_only_inside_the_left_right_margins() {
    // T:2082.
    let t = fed(5, 3, b"ABCD\r\nEFGH\x1b[?69h\x1b[2;4s\x1b[1;2H\x1bM");
    assert_eq!(text(&t), "A\nEBCD\n FGH");
    let t = fed(5, 3, b"ABCD\r\nEFGH\x1b[?69h\x1b[2;4s\x1b[1;1H\x1bM");
    assert_eq!(text(&t), "ABCD\nEFGH");
}

#[test]
fn screen_cr_in_origin_mode() {
    // T:1475.
    assert_eq!(cursor(&fed(10, 2, b"\x1b[?69h\x1b[3;8s\x1b[?6h\x1b[1;1H\x1b[5C\r")), (2, 0));
}

#[test]
fn screen_switching_ends_the_hyperlink_and_carries_the_charset() {
    let mut t = term(10, 2);
    t.screen_mut().start_hyperlink(b"https://example.com", None);
    t.feed(b"\x1b(0\x1b[?1049h`");
    assert_eq!(t.active(), Which::Alternate);
    assert_eq!(text(&t), "\u{25C6}");
    assert_eq!(t.screen().cell_link(cell(&t, 0, 0)), 0);
    assert_eq!(t.primary().cursor.hyperlink, 0);
}

#[test]
fn screen_alternate_has_no_scrollback_and_never_reflows() {
    let mut t = fed(5, 3, b"\x1b[?1049hABCDEFGH");
    t.resize(10, 3);
    assert_eq!(text(&t), "ABCDE\nFGH");
}

#[test]
fn screen_resize_rebuilds_tabs_and_resets_margins() {
    let mut t = fed(20, 5, b"\x1b[3g\x1b[1;4H\x1bH\x1b[2;4r\x1b[?69h\x1b[2;5s");
    t.resize(30, 6);
    assert!(t.is_tabstop(8) && !t.is_tabstop(3));
    let r = t.region();
    assert_eq!((r.top, r.bottom, r.left, r.right), (0, 5, 0, 29));
}

#[test]
fn screen_ris_resets_everything() {
    let mut t = fed(10, 3, b"A\x1b[>5u\x1b[1$}\x1b[?1049h\x1b[5 q");
    t.set_title(b"t");
    t.set_pwd(b"/x");
    t.feed(b"\x1bc");
    assert_eq!(t.title(), b"");
    assert_eq!(t.pwd(), b"");
    assert_eq!(t.previous_char(), None);
    assert_eq!(t.status_display(), StatusDisplay::Main);
    assert_eq!(t.screen().cursor.shape, CursorShape::Block);
    t.feed(b"\x1b[b\x1b[?u");
    assert_eq!(text(&t), "", "REP after RIS has nothing to repeat");
    assert_eq!(replies(&mut t), b"\x1b[?0u");
}

#[test]
fn screen_kitty_flags_are_per_screen() {
    let mut t = fed(10, 3, b"\x1b[>1u\x1b[?1049h\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?0u");
    t.feed(b"\x1b[>3u\x1b[?1049l\x1b[?u");
    assert_eq!(replies(&mut t), b"\x1b[?1u");
}

#[test]
fn screen_title_and_pwd_empty_clears() {
    let mut t = term(10, 2);
    t.set_title(b"x");
    t.set_title(b"");
    assert_eq!(t.title(), b"");
    t.set_pwd(b"/a");
    t.set_pwd(b"");
    assert_eq!(t.pwd(), b"");
    let e = effects(&mut t);
    assert_eq!(e, [Effect::TitleChanged, Effect::TitleChanged]);
}

#[test]
fn screen_scrollback_is_capped_in_bytes() {
    let mut o = opts(10, 3);
    o.scrollback_bytes = 2048;
    let mut t = Terminal::new_in(o, heap()).unwrap();
    for i in 0..500 {
        t.feed(std::format!("{i}\r\n").as_bytes());
    }
    let s = t.screen();
    assert!(s.pages.scrollback() < 100, "scrollback rows {}", s.pages.scrollback());
    assert!(s.pages.scrollback() > 0);
    assert_eq!(text(&t), "498\n499");
    // The pages kept stay within the cap plus the active area's pages.
    assert!(s.pages.bytes() <= 2048 + 2 * s.pages.page_bytes(), "bytes {}", s.pages.bytes());
}

#[test]
fn screen_zero_scrollback_keeps_at_most_one_page_more() {
    // A quirk kept: a cap of 0 still keeps enough pages for the active area and one more
    // (PageList.zig:491 minMaxSize), so a page of history survives.
    let mut o = opts(10, 2);
    o.scrollback_bytes = 0;
    let mut t = Terminal::new_in(o, heap()).unwrap();
    for i in 0..50 {
        t.feed(std::format!("{i}\r\n").as_bytes());
    }
    let s = t.screen();
    assert!(s.pages.scrollback() <= 2 * s.pages.page_rows(), "{}", s.pages.scrollback());
    assert_eq!(text(&t), "49");
}

#[test]
fn screen_dirty_rows_are_damage() {
    let mut t = term(10, 5);
    t.take_damage(|_| {});
    t.feed(b"\x1b[3;1HX");
    let mut rows = StdVec::new();
    let all = t.take_damage(|y| rows.push(y));
    assert!(!all);
    assert!(rows.contains(&2) && !rows.contains(&4), "{rows:?}");
    let mut again = StdVec::new();
    t.take_damage(|y| again.push(y));
    assert!(again.is_empty());
    t.feed(b"\x1b[2J");
    assert!(t.take_damage(|_| {}), "ED 2 damages everything");
}

#[test]
fn screen_styles_are_interned_and_released() {
    let mut t = fed(10, 2, b"\x1b[1mAB\x1b[0mC");
    assert_eq!(t.screen().styles.live(), 1);
    assert_eq!(cell(&t, 0, 0).style_id(), cell(&t, 1, 0).style_id());
    t.feed(b"\x1b[2J");
    // The cursor no longer holds bold, and the cells are gone.
    assert_eq!(t.screen().styles.live(), 0);
}

#[test]
fn screen_graphemes_are_released_on_overwrite() {
    let mut t = fed(10, 2, "e\u{301}".as_bytes());
    assert_eq!(text(&t), "e\u{301}");
    assert!(cell(&t, 0, 0).has_grapheme());
    t.feed(b"\x1b[HX");
    assert!(!cell(&t, 0, 0).has_grapheme());
    assert_eq!(text(&t), "X");
}

#[test]
fn screen_semantic_rows_default_to_none() {
    let t = term(10, 2);
    assert_eq!(t.screen().pages.row(t.screen().r(0)).prompt, RowPrompt::None);
}
