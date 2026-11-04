//! Terminal queries (cursor position, device attributes, colours, ...) are answered by the daemon
//! from its own emulator and stripped from the output stream. That way agents started with no
//! client attached don't hang waiting for a reply, and attached terminals never answer twice.

pub struct Filtered {
    /// Output with queries removed, to broadcast to clients.
    pub out: Vec<u8>,
    /// Replies to write back to the PTY.
    pub replies: Vec<u8>,
}

/// Feed `chunk` through the emulator, splitting at each recognised query so replies see the
/// cursor exactly where the app asked.
pub fn filter<CB: vt100::Callbacks>(chunk: &[u8], parser: &mut vt100::Parser<CB>) -> Filtered {
    let mut out = Vec::with_capacity(chunk.len());
    let mut replies = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] == 0x1b {
            if let Some(len) = query_len(&chunk[i..]) {
                let seg = &chunk[start..i];
                parser.process(seg);
                out.extend_from_slice(seg);
                replies.extend(reply(&chunk[i..i + len], parser.screen()));
                i += len;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    let seg = &chunk[start..];
    parser.process(seg);
    out.extend_from_slice(seg);
    Filtered { out, replies }
}

const FIXED: &[&[u8]] = &[
    b"\x1b[6n", b"\x1b[?6n", b"\x1b[5n", b"\x1b[c", b"\x1b[0c", b"\x1b[>c", b"\x1b[>0c", b"\x1b[?u",
    b"\x1b[18t", b"\x1b[14t", b"\x1b[16t", b"\x1b[>q", b"\x1b[>0q",
    b"\x1b]10;?\x1b\\", b"\x1b]10;?\x07", b"\x1b]11;?\x1b\\", b"\x1b]11;?\x07",
];

fn query_len(s: &[u8]) -> Option<usize> {
    if let Some(p) = FIXED.iter().find(|p| s.starts_with(p)) {
        return Some(p.len());
    }
    // DECRQM: ESC [ ? <digits> $ p
    if s.starts_with(b"\x1b[?") {
        let digits = s[3..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && s[3 + digits..].starts_with(b"$p") {
            return Some(3 + digits + 2);
        }
    }
    None
}

fn reply(q: &[u8], screen: &vt100::Screen) -> Vec<u8> {
    let (row, col) = screen.cursor_position();
    let (rows, cols) = screen.size();
    let s = match q {
        b"\x1b[6n" => format!("\x1b[{};{}R", row + 1, col + 1),
        b"\x1b[?6n" => format!("\x1b[?{};{}R", row + 1, col + 1),
        b"\x1b[5n" => "\x1b[0n".into(),
        b"\x1b[c" | b"\x1b[0c" => "\x1b[?62;22c".into(),
        b"\x1b[>c" | b"\x1b[>0c" => "\x1b[>0;10;1c".into(),
        b"\x1b[?u" => String::new(), // no kitty keyboard protocol; apps fall back after the DA1 reply
        b"\x1b[18t" => format!("\x1b[8;{rows};{cols}t"),
        b"\x1b[14t" => format!("\x1b[4;{};{}t", rows.get() as u32 * 16, cols.get() as u32 * 8),
        b"\x1b[16t" => "\x1b[6;16;8t".into(),
        b"\x1b[>q" | b"\x1b[>0q" => "\x1bP>|harness\x1b\\".into(),
        q if q.starts_with(b"\x1b]10;") => "\x1b]10;rgb:d8d8/d8d8/d8d8\x1b\\".into(),
        q if q.starts_with(b"\x1b]11;") => "\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\".into(),
        q => {
            // DECRQM: report synchronized output (2026) as supported-but-reset, everything else unknown.
            let mode = std::str::from_utf8(&q[3..q.len() - 2]).unwrap_or("0");
            let state = if mode == "2026" || mode == "2004" { 2 } else { 0 };
            format!("\x1b[?{mode};{state}$y")
        }
    };
    s.into_bytes()
}
