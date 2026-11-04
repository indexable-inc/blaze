//! Terminal queries (cursor position, device attributes, colours, ...) are answered by the daemon
//! from its own emulator and stripped from the output stream. That way agents started with no
//! client attached don't hang waiting for a reply, and attached terminals never answer twice.

pub struct Filtered {
    /// Output with queries removed, to broadcast to clients.
    pub out: Vec<u8>,
    /// Replies to write back to the PTY.
    pub replies: Vec<u8>,
}

/// Feed `chunk` through the emulator, splitting at each recognised query so it is answered with the cursor exactly
/// where the app asked. libghostty answers each query itself (configured in harness-vt to give the replies the daemon
/// always gave); anything it would answer that is not stripped here is left to the attached client, as before.
pub fn filter(chunk: &[u8], term: &mut harness_vt::Term) -> Filtered {
    let mut out = Vec::with_capacity(chunk.len());
    let mut replies = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] == 0x1b {
            if let Some(len) = query_len(&chunk[i..]) {
                let seg = &chunk[start..i];
                term.feed(seg);
                out.extend_from_slice(seg);
                let q = &chunk[i..i + len];
                let r = term.query(q);
                // No kitty keyboard protocol: clients send legacy keys, so apps must fall back (after the DA1 reply).
                if q != b"\x1b[?u" {
                    replies.extend(r);
                }
                i += len;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    let seg = &chunk[start..];
    term.feed(seg);
    out.extend_from_slice(seg);
    Filtered { out, replies }
}

const FIXED: &[&[u8]] = &[
    b"\x1b[6n", b"\x1b[?6n", b"\x1b[5n", b"\x1b[c", b"\x1b[0c", b"\x1b[>c", b"\x1b[>0c", b"\x1b[?u",
    b"\x1b[18t", b"\x1b[14t", b"\x1b[16t", b"\x1b[>q", b"\x1b[>0q",
    b"\x1b]10;?\x1b\\", b"\x1b]10;?\x07", b"\x1b]11;?\x1b\\", b"\x1b]11;?\x07", b"\x1b]12;?\x1b\\", b"\x1b]12;?\x07",
    // The appearance (dark or light), which picks the Ghostty theme's half the colours above come from.
    b"\x1b[?996n",
];

fn query_len(s: &[u8]) -> Option<usize> {
    if let Some(p) = FIXED.iter().find(|p| s.starts_with(p)) {
        return Some(p.len());
    }
    // OSC 4 palette queries: ESC ] 4 ; <index> ; ? [; <index> ; ?]... then BEL or ST.
    if let Some(mut rest) = s.strip_prefix(b"\x1b]4;") {
        loop {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits == 0 || !rest[digits..].starts_with(b";?") {
                break;
            }
            rest = &rest[digits + 2..];
            if let Some(after) = rest.strip_prefix(b";") {
                rest = after;
                continue;
            }
            let end = if rest.starts_with(b"\x07") {
                1
            } else if rest.starts_with(b"\x1b\\") {
                2
            } else {
                break;
            };
            return Some(s.len() - rest.len() + end);
        }
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
