//! The parser against ghostty's Parser.zig and stream.zig tests (each names its original) and
//! the quirks vt-inventory.md §1–§2 lists, plus a property: any split of the input into feeds
//! gives the same events as one feed.

use std::vec::Vec;
use std::{format, vec};

use crate::parser::{Csi, Dcs, Esc, MAX_PARAMS, Parser, Sink, Terminator};

#[derive(Clone, PartialEq, Eq, Debug)]
enum Ev {
    Print(char),
    Exec(u8),
    Esc(Vec<u8>, u8),
    Csi { inter: Vec<u8>, params: Vec<u16>, colons: u32, fin: u8 },
    Osc(Vec<u8>, Terminator),
    DcsHook(Vec<u8>, Vec<u16>, u8),
    DcsPut(u8),
    DcsUnhook,
    Apc(Vec<u8>),
}

#[derive(Default)]
struct Rec {
    evs: Vec<Ev>,
    osc: Option<Vec<u8>>,
    apc: Option<Vec<u8>>,
}

impl Sink for Rec {
    fn print(&mut self, c: char) {
        self.evs.push(Ev::Print(c));
    }
    fn execute(&mut self, c0: u8) {
        self.evs.push(Ev::Exec(c0));
    }
    fn esc(&mut self, esc: &Esc<'_>) {
        self.evs.push(Ev::Esc(esc.intermediates.to_vec(), esc.final_byte));
    }
    fn csi(&mut self, csi: &Csi<'_>) {
        self.evs.push(Ev::Csi {
            inter: csi.intermediates.to_vec(),
            params: csi.params.to_vec(),
            colons: csi.colons,
            fin: csi.final_byte,
        });
    }
    fn osc_start(&mut self) {
        assert!(self.osc.is_none(), "osc_start twice");
        self.osc = Some(Vec::new());
    }
    fn osc_put(&mut self, b: u8) {
        self.osc.as_mut().expect("osc_put outside an OSC").push(b);
    }
    fn osc_end(&mut self, end: Terminator) {
        let data = self.osc.take().expect("osc_end outside an OSC");
        self.evs.push(Ev::Osc(data, end));
    }
    fn dcs_hook(&mut self, dcs: &Dcs<'_>) {
        self.evs.push(Ev::DcsHook(dcs.intermediates.to_vec(), dcs.params.to_vec(), dcs.final_byte));
    }
    fn dcs_put(&mut self, b: u8) {
        self.evs.push(Ev::DcsPut(b));
    }
    fn dcs_unhook(&mut self) {
        self.evs.push(Ev::DcsUnhook);
    }
    fn apc_start(&mut self) {
        assert!(self.apc.is_none(), "apc_start twice");
        self.apc = Some(Vec::new());
    }
    fn apc_put(&mut self, bytes: &[u8]) {
        self.apc.as_mut().expect("apc_put outside an APC").extend_from_slice(bytes);
    }
    fn apc_end(&mut self) {
        let data = self.apc.take().expect("apc_end outside an APC");
        self.evs.push(Ev::Apc(data));
    }
}

fn run(bytes: &[u8]) -> Vec<Ev> {
    let mut p = Parser::new();
    let mut r = Rec::default();
    p.feed(bytes, &mut r);
    r.evs
}

fn csi(inter: &[u8], params: &[u16], colons: u32, fin: u8) -> Ev {
    Ev::Csi { inter: inter.to_vec(), params: params.to_vec(), colons, fin }
}

fn prints(s: &str) -> Vec<Ev> {
    s.chars().map(Ev::Print).collect()
}

#[test]
fn esc_designate_g0_ascii() {
    // Parser.zig:441 "esc: ESC ( B"
    assert_eq!(run(b"\x1b(B"), vec![Ev::Esc(b"(".to_vec(), b'B')]);
}

#[test]
fn csi_cup_without_params() {
    // Parser.zig:460 "csi: ESC [ H"
    assert_eq!(run(b"\x1b[H"), vec![csi(b"", &[], 0, b'H')]);
}

#[test]
fn csi_cup_with_params() {
    // Parser.zig:478 "csi: ESC [ 1 ; 4 H"
    assert_eq!(run(b"\x1b[1;4H"), vec![csi(b"", &[1, 4], 0, b'H')]);
}

#[test]
fn csi_sgr_colon_sets_the_separator_bit() {
    // Parser.zig:501 "csi: SGR ESC [ 38 : 2 m"
    assert_eq!(run(b"\x1b[38:2m"), vec![csi(b"", &[38, 2], 0b1, b'm')]);
}

#[test]
fn csi_sgr_underline_color_with_many_colons() {
    // Parser.zig:633 "csi: SGR with many blank and colon"
    assert_eq!(
        run(b"\x1b[58:2::240:143:104m"),
        vec![csi(b"", &[58, 2, 0, 240, 143, 104], 0b11111, b'm')]
    );
}

#[test]
fn csi_colon_for_a_final_other_than_m_drops_it() {
    // Parser.zig:778 "csi: colon for non-m final"
    assert_eq!(run(b"\x1b[38:2h"), vec![]);
    assert_eq!(run(b"\x1b[38:2h\x1b[H"), vec![csi(b"", &[], 0, b'H')]);
}

#[test]
fn csi_private_marker_is_an_intermediate() {
    // Parser.zig:791 "csi: request mode decrqm"
    assert_eq!(run(b"\x1b[?2026$p"), vec![csi(b"?$", &[2026], 0, b'p')]);
}

#[test]
fn csi_decscusr() {
    // Parser.zig:818 "csi: change cursor"
    assert_eq!(run(b"\x1b[3 q"), vec![csi(b" ", &[3], 0, b'q')]);
}

#[test]
fn csi_too_many_params_drops_the_sequence() {
    // Parser.zig:961 "csi: too many params"
    let mut bytes = b"\x1b[".to_vec();
    for _ in 0..100 {
        bytes.extend_from_slice(b"1;");
    }
    bytes.extend_from_slice(b"1C");
    assert_eq!(run(&bytes), vec![]);
}

#[test]
fn csi_up_to_max_params_dispatches() {
    // Parser.zig:980 "csi: sgr with up to our max parameters"
    for max in 1..=MAX_PARAMS {
        let mut bytes = b"\x1b[".to_vec();
        for _ in 0..max - 1 {
            bytes.extend_from_slice(b"1;");
        }
        bytes.extend_from_slice(b"2H");
        let mut params = vec![1u16; max - 1];
        params.push(2);
        assert_eq!(run(&bytes), vec![csi(b"", &params, 0, b'H')], "max = {max}");
    }
}

#[test]
fn csi_one_past_max_params_drops_it() {
    // Parser.zig:1006 "csi: sgr beyond our max drops it"
    let mut bytes = b"\x1b[".to_vec();
    for _ in 0..MAX_PARAMS {
        bytes.extend_from_slice(b"1;");
    }
    bytes.extend_from_slice(b"2m");
    assert_eq!(run(&bytes), vec![]);
}

#[test]
fn csi_param_saturates() {
    // stream.zig:3449 "stream: csi param too long"
    assert_eq!(run(b"\x1b[99999999999999999999C"), vec![csi(b"", &[u16::MAX], 0, b'C')]);
}

#[test]
fn csi_empty_params_are_zero_and_a_trailing_empty_one_is_dropped() {
    assert_eq!(run(b"\x1b[;5H"), vec![csi(b"", &[0, 5], 0, b'H')]);
    assert_eq!(run(b"\x1b[5;H"), vec![csi(b"", &[5], 0, b'H')]);
}

#[test]
fn csi_more_than_four_intermediates_drops_the_extra_and_dispatches() {
    assert_eq!(run(b"\x1b[?$$$$$p"), vec![csi(b"?$$$", &[], 0, b'p')]);
}

#[test]
fn csi_marker_after_a_param_ignores_the_sequence() {
    assert_eq!(run(b"\x1b[1?hA"), vec![Ev::Print('A')]);
}

#[test]
fn csi_executes_c0_inside() {
    assert_eq!(run(b"\x1b[1\x08C"), vec![Ev::Exec(0x08), csi(b"", &[1], 0, b'C')]);
}

#[test]
fn osc_title_ended_by_bel_and_by_st() {
    // Parser.zig:844 "osc: change window title", :867 "(end in esc)"
    assert_eq!(run(b"\x1b]2;abc\x07"), vec![Ev::Osc(b"2;abc".to_vec(), Terminator::Bel)]);
    assert_eq!(
        run(b"\x1b]2;abc\x1b\\"),
        vec![Ev::Osc(b"2;abc".to_vec(), Terminator::St), Ev::Esc(vec![], b'\\')]
    );
}

#[test]
fn osc_ended_by_can_still_dispatches() {
    // vt-inventory.md §1: leaving osc_string dispatches on any exit.
    assert_eq!(
        run(b"\x1b]0;t\x18A"),
        vec![Ev::Osc(b"0;t".to_vec(), Terminator::St), Ev::Exec(0x18), Ev::Print('A')]
    );
}

#[test]
fn osc_c1_st_is_data_not_a_terminator() {
    // vt-inventory.md §1: 0x9C is DATA in osc_string.
    assert_eq!(run(b"\x1b]0;a\x9cb\x07"), vec![Ev::Osc(b"0;a\x9cb".to_vec(), Terminator::Bel)]);
}

#[test]
fn osc_ignores_other_c0() {
    assert_eq!(run(b"\x1b]0;a\x01b\x07"), vec![Ev::Osc(b"0;ab".to_vec(), Terminator::Bel)]);
}

#[test]
fn dcs_xtgettcap() {
    // Parser.zig:1029 "dcs: XTGETTCAP"
    let mut want = vec![Ev::DcsHook(b"+".to_vec(), vec![], b'q')];
    want.extend(b"536D756C78".iter().map(|&b| Ev::DcsPut(b)));
    want.push(Ev::DcsUnhook);
    want.push(Ev::Esc(vec![], b'\\'));
    assert_eq!(run(b"\x1bP+q536D756C78\x1b\\"), want);
}

#[test]
fn dcs_params() {
    // Parser.zig:1053 "dcs: params"
    assert_eq!(run(b"\x1bP1000p\x1b\\")[0], Ev::DcsHook(vec![], vec![1000], b'p'));
}

#[test]
fn dcs_too_many_params_is_never_hooked() {
    // Parser.zig:1076 "dcs: too many params"
    let mut bytes = b"\x1bP".to_vec();
    for _ in 0..MAX_PARAMS {
        bytes.extend_from_slice(b"1;");
    }
    bytes.extend_from_slice(b"1pab\x1b\\");
    assert_eq!(run(&bytes), vec![Ev::Esc(vec![], b'\\')]);
}

#[test]
fn apc_bulk_payload_keeps_c0_and_ends_at_st() {
    // stream.zig:3868 "apc bulk slice", :3900 "keeps C0 bytes as data"
    assert_eq!(
        run(b"\x1b_Gab\x01c\x1b\\"),
        vec![Ev::Apc(b"Gab\x01c".to_vec()), Ev::Esc(vec![], b'\\')]
    );
}

#[test]
fn apc_aborted_by_can() {
    // stream.zig:3909 "apc aborted by CAN"
    assert_eq!(
        run(b"\x1b_Gab\x18Z"),
        vec![Ev::Apc(b"Gab".to_vec()), Ev::Exec(0x18), Ev::Print('Z')]
    );
}

#[test]
fn sos_and_pm_are_apc() {
    assert_eq!(run(b"\x1bXs\x1b\\")[0], Ev::Apc(b"s".to_vec()));
    assert_eq!(run(b"\x1b^p\x1b\\")[0], Ev::Apc(b"p".to_vec()));
}

#[test]
fn ground_c0_up_to_0x0f_executes_and_the_rest_print() {
    // Terminal.zig:12561: 0x10–0x1F (but ESC) and DEL are printed in ground.
    assert_eq!(
        run(b"\x07\x0f\x10\x18\x1a\x1f\x7f"),
        vec![
            Ev::Exec(0x07),
            Ev::Exec(0x0f),
            Ev::Print('\u{10}'),
            Ev::Print('\u{18}'),
            Ev::Print('\u{1a}'),
            Ev::Print('\u{1f}'),
            Ev::Print('\u{7f}'),
        ]
    );
}

#[test]
fn ground_raw_c1_is_invalid_utf8() {
    // vt-inventory.md §2: 8-bit C1 in ground becomes U+FFFD.
    assert_eq!(run(b"\x9b1m"), vec![Ev::Print('\u{FFFD}'), Ev::Print('1'), Ev::Print('m')]);
}

#[test]
fn ground_utf8_c1_code_point_prints() {
    assert_eq!(run("\u{9b}".as_bytes()), vec![Ev::Print('\u{9b}')]);
}

#[test]
fn c1_mid_sequence_executes_as_esc() {
    // ESC [ then 0x84 (IND): the CSI is abandoned and IND runs as ESC D.
    assert_eq!(run(b"\x1b[\x84"), vec![Ev::Esc(vec![], b'D')]);
    // 0x9B mid-sequence enters CSI.
    assert_eq!(run(b"\x1b(\x9b2J"), vec![csi(b"", &[2], 0, b'J')]);
}

#[test]
fn utf8_invalid_bytes_and_cut_sequences() {
    // stream.zig:2788 "simd: print invalid utf-8", :2810 "complete incomplete utf-8"
    assert_eq!(run(&[0xFF]), vec![Ev::Print('\u{FFFD}')]);
    assert_eq!(run(&[0xE0, 0xA0, 0x80]), vec![Ev::Print('\u{800}')]);
    // UTF8Decoder.zig:141: a cut sequence is one U+FFFD and the breaking byte is read again.
    assert_eq!(
        run(&[0xF0, 0x9F, 0x98, 0x84, 0xF0, 0x9F, 0x41]),
        vec![Ev::Print('\u{1F604}'), Ev::Print('\u{FFFD}'), Ev::Print('A'),]
    );
    // ESC cuts a sequence and still starts one.
    assert_eq!(
        run(&[0xE2, 0x82, 0x1B, b'[', b'H']),
        vec![Ev::Print('\u{FFFD}'), csi(b"", &[], 0, b'H')]
    );
    // Surrogates and overlongs are refused.
    assert_eq!(
        run(&[0xED, 0xA0, 0x80]),
        vec![Ev::Print('\u{FFFD}'), Ev::Print('\u{FFFD}'), Ev::Print('\u{FFFD}')]
    );
    assert_eq!(run(&[0xC0, 0xAF]), vec![Ev::Print('\u{FFFD}'), Ev::Print('\u{FFFD}')]);
}

#[test]
fn esc_intermediate_and_del_ignored() {
    assert_eq!(run(b"\x1b#\x7f8"), vec![Ev::Esc(b"#".to_vec(), b'8')]);
}

#[test]
fn at_rest_only_in_ground_between_code_points() {
    let mut p = Parser::new();
    let mut r = Rec::default();
    assert!(p.at_rest());
    p.feed(&[0xE2], &mut r);
    assert!(!p.at_rest());
    p.feed(&[0x82, 0xAC, 0x1B], &mut r);
    assert!(!p.at_rest());
    p.feed(b"[m", &mut r);
    assert!(p.at_rest());
}

/// Bytes the property draws from: every byte class the table distinguishes.
const ALPHABET: &[u8] = &[
    0x00, 0x07, 0x08, 0x0A, 0x0D, 0x10, 0x18, 0x1A, 0x1B, 0x1F, b' ', b'!', b'"', b'$', b'+', b'0',
    b'5', b'9', b':', b';', b'<', b'?', b'@', b'A', b'H', b'P', b'X', b'[', b'\\', b']', b'^',
    b'_', b'm', b'q', b'~', 0x7F, 0x84, 0x90, 0x98, 0x9B, 0x9C, 0x9D, 0xA0, 0xC3, 0xE2, 0xF0, 0xFF,
];

#[test]
fn any_split_into_feeds_gives_the_events_of_one_feed() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let bytes = tape.vec(0..=64, "bytes", |t| *t.pick(ALPHABET, "byte").unwrap_or(&0));
        let whole = run(&bytes);
        let mut p = Parser::new();
        let mut r = Rec::default();
        let mut at = 0;
        while at < bytes.len() {
            let n = tape.int_in(1..=bytes.len() - at, "chunk");
            p.feed(&bytes[at..at + n], &mut r);
            at += n;
        }
        assert_eq!(r.evs, whole, "{}", format!("{bytes:02x?}"));
    });
}

#[test]
fn a_feed_never_panics_and_leaves_ground_reachable() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let bytes = tape.vec(0..=128, "bytes", |t| *t.pick(ALPHABET, "byte").unwrap_or(&0));
        let mut p = Parser::new();
        let mut r = Rec::default();
        p.feed(&bytes, &mut r);
        // CAN always returns to ground (from a UTF-8 sequence too, via U+FFFD and print).
        p.feed(b"\x18", &mut r);
        r.evs.clear();
        p.feed(b"Z", &mut r);
        assert_eq!(r.evs, prints("Z"));
    });
}
