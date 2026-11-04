//! A fresh terminal fed `vt_snapshot` shows what the original shows: cells, cursor and modes.
use harness_vt::{CursorShape, Term, UnderlineStyle};

fn replay(src: &mut Term) -> Term {
    let (c, r) = src.size();
    let mut dst = Term::new(c, r, 100);
    dst.feed(&src.vt_snapshot());
    dst
}

#[test]
fn snapshot_rebuilds_screen_cursor_and_modes() {
    let mut a = Term::new(20, 6, 100);
    a.feed(b"\x1b[?2027h"); // grapheme clustering (off by default, like vt100 and xterm.js)
    a.feed(b"\x1b[38;2;255;100;0mtrue\x1b[0m \x1b[4:3;58;5;196mcurly\x1b[0m\r\n");
    a.feed("世界 👨\u{200d}👩\u{200d}👧 ▀▄█\r\n".as_bytes());
    a.feed(b"\x1b]8;;https://x.dev\x1b\\link\x1b]8;;\x1b\\ \x1b[7minv\x1b[0m\r\n");
    a.feed(b"\x1b[?2004h\x1b[?1006h\x1b[?1002h\x1b[?1h\x1b[5 q\x1b]2;hello\x07\x1b[3;7H");
    let mut b = replay(&mut a);
    let (ra, ca) = a.rows();
    let (rb, cb) = b.rows();
    for (y, (x, z)) in ra.iter().zip(&rb).enumerate() {
        for (i, (p, q)) in x.cells.iter().zip(&z.cells).enumerate() {
            assert_eq!(p, q, "row {y} col {i}");
        }
    }
    assert_eq!(ca, cb);
    assert_eq!(a.modes(), b.modes());
    assert_eq!(b.title(), "hello");
    let c = b.cursor();
    assert_eq!((c.x, c.y, c.shape, c.blinking, c.explicit), (6, 2, CursorShape::Bar, true, true));
    assert_eq!(ra[0].cells[5].underline, UnderlineStyle::Curly);
    assert_eq!(ra[1].cells[0].text, "世");
    assert!(ra[1].cells[0].wide && ra[1].cells[1].spacer);
    assert_eq!(ra[1].cells[5].text, "👨\u{200d}👩\u{200d}👧");
    assert_eq!(ra[2].cells[0].link.as_deref(), Some("https://x.dev"));
}

#[test]
fn default_cursor_is_not_explicit_and_alt_screen_survives() {
    let mut a = Term::new(10, 4, 10);
    assert!(!a.cursor().explicit);
    a.feed(b"\x1b[2 q");
    assert!(a.cursor().explicit && !a.cursor().blinking);
    a.feed(b"\x1b[0 q\x1b[?1049hALT\x1b[?25l");
    assert!(!a.cursor().explicit);
    let mut b = replay(&mut a);
    assert!(b.alternate_screen());
    assert!(!b.cursor().visible);
    assert_eq!(b.text(), "ALT");
}

#[test]
fn queries_answer_like_the_daemon_did() {
    let mut t = Term::new(80, 24, 10);
    t.feed(b"ab");
    assert_eq!(t.query(b"\x1b[6n"), b"\x1b[1;3R");
    assert_eq!(t.query(b"\x1b[c"), b"\x1b[?62;22c");
    assert_eq!(t.query(b"\x1b[>c"), b"\x1b[>0;10;1c");
    assert_eq!(t.query(b"\x1b[>q"), b"\x1bP>|harness\x1b\\");
    assert_eq!(t.query(b"\x1b[18t"), b"\x1b[8;24;80t");
    assert_eq!(t.query(b"\x1b[14t"), b"\x1b[4;384;640t");
    assert_eq!(t.query(b"\x1b]11;?\x1b\\"), b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\");
}
