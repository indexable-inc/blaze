//! HTTP/3's frames (RFC 9114, 7), its SETTINGS, the stream types and signals WebTransport adds
//! (draft-ietf-webtrans-http3), the capsules a session's CONNECT stream carries (RFC 9297), and
//! the error codes of each.

#![blaze::sans_io]

use super::varint;

/// Frame types.
pub(crate) const DATA: u64 = 0x00;
pub(crate) const HEADERS: u64 = 0x01;
pub(crate) const CANCEL_PUSH: u64 = 0x03;
pub(crate) const SETTINGS: u64 = 0x04;
pub(crate) const PUSH_PROMISE: u64 = 0x05;
pub(crate) const GOAWAY: u64 = 0x07;
pub(crate) const MAX_PUSH_ID: u64 = 0x0d;
/// Not a frame: the signal a WebTransport bidirectional stream starts with, followed by its
/// session's ID and then the stream's bytes, with no length.
pub(crate) const WEBTRANSPORT_STREAM: u64 = 0x41;

/// Unidirectional stream types.
pub(crate) const STREAM_CONTROL: u64 = 0x00;
pub(crate) const STREAM_PUSH: u64 = 0x01;
pub(crate) const STREAM_QPACK_ENCODER: u64 = 0x02;
pub(crate) const STREAM_QPACK_DECODER: u64 = 0x03;
pub(crate) const STREAM_WEBTRANSPORT: u64 = 0x54;

/// Settings.
pub(crate) const QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
pub(crate) const QPACK_BLOCKED_STREAMS: u64 = 0x07;
pub(crate) const ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
pub(crate) const H3_DATAGRAM: u64 = 0x33;
/// draft-02's.
pub(crate) const ENABLE_WEBTRANSPORT: u64 = 0x2b60_3742;
/// draft-07's.
pub(crate) const WEBTRANSPORT_MAX_SESSIONS: u64 = 0xc671_706a;

/// What a blaze endpoint advertises, in this order: what wtransport 0.7.2 sent, which Chrome's
/// draft-02 and draft-07 clients take (docs/v2/plan/design/http3-sans-io.md, section 4).
pub(crate) const OURS: [(u64, u64); 6] = [
    (QPACK_MAX_TABLE_CAPACITY, 0),
    (QPACK_BLOCKED_STREAMS, 0),
    (ENABLE_CONNECT_PROTOCOL, 1),
    (H3_DATAGRAM, 1),
    (ENABLE_WEBTRANSPORT, 1),
    (WEBTRANSPORT_MAX_SESSIONS, 1),
];

/// Capsules.
pub(crate) const CLOSE_WEBTRANSPORT_SESSION: u64 = 0x2843;
pub(crate) const DRAIN_WEBTRANSPORT_SESSION: u64 = 0x78ae;

/// HTTP/3's error codes (RFC 9114, 8.1; RFC 9204, 6; RFC 9297; the WebTransport draft).
pub(crate) mod code {
    pub(crate) const H3_NO_ERROR: u64 = 0x100;
    pub(crate) const H3_STREAM_CREATION_ERROR: u64 = 0x103;
    pub(crate) const H3_CLOSED_CRITICAL_STREAM: u64 = 0x104;
    pub(crate) const H3_FRAME_UNEXPECTED: u64 = 0x105;
    pub(crate) const H3_FRAME_ERROR: u64 = 0x106;
    pub(crate) const H3_EXCESSIVE_LOAD: u64 = 0x107;
    pub(crate) const H3_ID_ERROR: u64 = 0x108;
    pub(crate) const H3_SETTINGS_ERROR: u64 = 0x109;
    pub(crate) const H3_MISSING_SETTINGS: u64 = 0x10a;
    pub(crate) const H3_MESSAGE_ERROR: u64 = 0x10e;
    pub(crate) const QPACK_DECOMPRESSION_FAILED: u64 = 0x200;
    pub(crate) const H3_DATAGRAM_ERROR: u64 = 0x33;
    pub(crate) const WEBTRANSPORT_BUFFERED_STREAM_REJECTED: u64 = 0x3994_bd84;
    pub(crate) const WEBTRANSPORT_SESSION_GONE: u64 = 0x170d_7b68;
}

/// The first and last HTTP/3 codes WebTransport's 32-bit application codes map to.
const WT_FIRST: u64 = 0x52e4_a40f_a8db;
const WT_LAST: u64 = WT_FIRST + 0xffff_ffff + 0xffff_ffff / 0x1e;

/// A WebTransport application code as HTTP/3 carries it in RESET_STREAM and STOP_SENDING: the
/// draft's mapping, which skips HTTP/3's reserved (GREASE) codes.
pub(crate) fn wt_to_h3(code: u32) -> u64 {
    let n = u64::from(code);
    WT_FIRST + n + n / 0x1e
}

/// The WebTransport application code an HTTP/3 code carries, if it is one.
pub(crate) fn h3_to_wt(code: u64) -> Option<u32> {
    if !(WT_FIRST..=WT_LAST).contains(&code) || (code - 0x21) % 0x1f == 0 {
        return None;
    }
    let shifted = code - WT_FIRST;
    u32::try_from(shifted - shifted / 0x1f).ok()
}

/// The most bytes of a frame the core holds to read it whole. A CONNECT's field section, a
/// SETTINGS frame and a capsule are each far under it; a peer's frame past it is
/// `H3_EXCESSIVE_LOAD`.
pub(crate) const MOST_FRAME: u64 = 64 << 10;

/// One frame at the start of a stream's buffered bytes.
#[derive(Debug)]
pub(crate) struct Frame<'a> {
    pub(crate) ty: u64,
    pub(crate) payload: &'a [u8],
    /// The bytes the frame took, header and payload.
    pub(crate) len: usize,
}

/// Why a stream's bytes are not frames.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FrameError {
    pub(crate) code: u64,
    pub(crate) why: String,
}

/// The frame at the start of `bytes`: `Ok(None)` while it is not all there.
pub(crate) fn next(bytes: &[u8]) -> Result<Option<Frame<'_>>, FrameError> {
    let Some((ty, a)) = varint::get(bytes) else { return Ok(None) };
    let Some((len, b)) = varint::get(&bytes[a..]) else { return Ok(None) };
    if len > MOST_FRAME {
        return Err(FrameError {
            code: code::H3_EXCESSIVE_LOAD,
            // alloc: an error's message, made only when it fails.
            why: format!(
                "a frame of type {ty:#x} is {len} bytes, past the {MOST_FRAME} this endpoint reads"
            ),
        });
    }
    let start = a + b;
    let end = start + len as usize;
    let Some(payload) = bytes.get(start..end) else { return Ok(None) };
    Ok(Some(Frame { ty, payload, len: end }))
}

/// Appends a frame of `ty` around `payload`.
pub(crate) fn put(out: &mut Vec<u8>, ty: u64, payload: &[u8]) {
    varint::put(out, ty);
    varint::put(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// The SETTINGS frame of [`OURS`].
pub(crate) fn put_settings(out: &mut Vec<u8>) {
    // alloc: one SETTINGS payload a connection.
    let mut payload = Vec::with_capacity(32);
    for (id, value) in OURS {
        varint::put(&mut payload, id);
        varint::put(&mut payload, value);
    }
    put(out, SETTINGS, &payload);
}

/// A peer's SETTINGS, as far as the core reads them: unknown ids are ignored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub qpack_max_table_capacity: u64,
    pub qpack_blocked_streams: u64,
    pub enable_connect_protocol: bool,
    pub h3_datagram: bool,
    pub enable_webtransport: bool,
    pub webtransport_max_sessions: u64,
}

impl Settings {
    /// Whether a server with these settings takes WebTransport sessions: extended CONNECT,
    /// HTTP/3 datagrams, and draft-02's or draft-07's WebTransport setting.
    pub fn serves_webtransport(&self) -> Result<(), &'static str> {
        if !self.enable_connect_protocol {
            return Err("SETTINGS_ENABLE_CONNECT_PROTOCOL is not 1");
        }
        if !self.h3_datagram {
            return Err("SETTINGS_H3_DATAGRAM is not 1");
        }
        if !self.enable_webtransport && self.webtransport_max_sessions == 0 {
            return Err(
                "neither SETTINGS_ENABLE_WEBTRANSPORT nor SETTINGS_WEBTRANSPORT_MAX_SESSIONS is set",
            );
        }
        Ok(())
    }
}

/// The settings in a SETTINGS frame's payload.
pub(crate) fn parse_settings(payload: &[u8]) -> Result<Settings, FrameError> {
    let fail = |code, why: &str| FrameError {
        code,
        // alloc: an error's message, made only when it fails.
        why: why.to_owned(),
    };
    let mut settings = Settings::default();
    // alloc: the ids seen in one SETTINGS frame, to refuse a repeat.
    let mut seen = Vec::new();
    let mut at = 0;
    while at < payload.len() {
        let Some((id, a)) = varint::get(&payload[at..]) else {
            return Err(fail(code::H3_FRAME_ERROR, "a SETTINGS frame ends inside an id"));
        };
        let Some((value, b)) = varint::get(&payload[at + a..]) else {
            return Err(fail(code::H3_FRAME_ERROR, "a SETTINGS frame ends inside a value"));
        };
        at += a + b;
        if matches!(id, 0x00 | 0x02..=0x05) {
            return Err(FrameError {
                code: code::H3_SETTINGS_ERROR,
                // alloc: an error's message, made only when it fails.
                why: format!("SETTINGS holds {id:#x}, reserved as HTTP/2's (RFC 9114, 7.2.4.1)"),
            });
        }
        if seen.contains(&id) {
            return Err(FrameError {
                code: code::H3_SETTINGS_ERROR,
                // alloc: an error's message, made only when it fails.
                why: format!("SETTINGS holds {id:#x} twice"),
            });
        }
        seen.push(id);
        match id {
            QPACK_MAX_TABLE_CAPACITY => settings.qpack_max_table_capacity = value,
            QPACK_BLOCKED_STREAMS => settings.qpack_blocked_streams = value,
            ENABLE_CONNECT_PROTOCOL => settings.enable_connect_protocol = value == 1,
            H3_DATAGRAM => settings.h3_datagram = value == 1,
            ENABLE_WEBTRANSPORT => settings.enable_webtransport = value == 1,
            WEBTRANSPORT_MAX_SESSIONS => settings.webtransport_max_sessions = value,
            // Unknown and GREASE ids, and MAX_FIELD_SECTION_SIZE (the core's own bound is
            // MOST_FRAME): ignored, as RFC 9114 7.2.4 says.
            _ => {}
        }
    }
    Ok(settings)
}

/// One capsule at the start of a CONNECT stream's DATA bytes: its type and payload, and the
/// bytes it took; `None` while it is not all there.
pub(crate) fn next_capsule(bytes: &[u8]) -> Option<(u64, &[u8], usize)> {
    let (ty, a) = varint::get(bytes)?;
    let (len, b) = varint::get(&bytes[a..])?;
    let start = a + b;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    Some((ty, bytes.get(start..end)?, end))
}

/// The DATA frame of a CLOSE_WEBTRANSPORT_SESSION capsule with `code` and `reason` (at most
/// 1024 bytes, cut at a character boundary).
pub(crate) fn put_close(out: &mut Vec<u8>, code: u32, reason: &str) {
    let mut cut = reason.len().min(1024);
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    // alloc: one capsule, as a session closes.
    let mut capsule = Vec::with_capacity(16 + cut);
    varint::put(&mut capsule, CLOSE_WEBTRANSPORT_SESSION);
    varint::put(&mut capsule, 4 + cut as u64);
    capsule.extend_from_slice(&code.to_be_bytes());
    capsule.extend_from_slice(&reason.as_bytes()[..cut]);
    put(out, DATA, &capsule);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webtransport_codes_map_both_ways_and_skip_grease() {
        assert_eq!(wt_to_h3(0), WT_FIRST);
        assert_eq!(wt_to_h3(0x1d), WT_FIRST + 0x1d);
        assert_eq!(wt_to_h3(0x1e), WT_FIRST + 0x1f, "one past 0x1d skips a GREASE code");
        assert_eq!(wt_to_h3(u32::MAX), WT_LAST);
        for code in [0, 1, 0x1d, 0x1e, 0x1f, 1000, 0xdead_beef, u32::MAX] {
            let h3 = wt_to_h3(code);
            assert_ne!((h3 - 0x21) % 0x1f, 0, "{code} maps onto a GREASE code");
            assert_eq!(h3_to_wt(h3), Some(code));
        }
        assert_eq!(h3_to_wt(code::H3_NO_ERROR), None);
        assert_eq!(h3_to_wt(WT_FIRST + 0x1e), None, "the GREASE code inside the range");
    }

    #[test]
    fn our_settings_parse_back_and_reserved_or_repeated_ids_are_refused() {
        let mut out = Vec::new();
        put_settings(&mut out);
        let frame = next(&out).unwrap().unwrap();
        assert_eq!((frame.ty, frame.len), (SETTINGS, out.len()));
        let settings = parse_settings(frame.payload).unwrap();
        assert_eq!(settings.serves_webtransport(), Ok(()));
        assert_eq!((settings.qpack_max_table_capacity, settings.webtransport_max_sessions), (0, 1));
        assert!(next(&out[..out.len() - 1]).unwrap().is_none(), "a cut frame waits for the rest");

        // A GREASE id (0x21 + 0x1f) is ignored.
        assert_eq!(parse_settings(&[0x40, 0x40, 0x05]).unwrap(), Settings::default());
        assert_eq!(parse_settings(&[0x02, 0x00]).unwrap_err().code, code::H3_SETTINGS_ERROR);
        assert_eq!(
            parse_settings(&[0x33, 0x01, 0x33, 0x01]).unwrap_err().code,
            code::H3_SETTINGS_ERROR
        );
        assert_eq!(parse_settings(&[0x33]).unwrap_err().code, code::H3_FRAME_ERROR);
    }

    #[test]
    fn a_close_capsule_reads_back() {
        let mut out = Vec::new();
        put_close(&mut out, 1, "bad_token");
        let frame = next(&out).unwrap().unwrap();
        assert_eq!(frame.ty, DATA);
        let (ty, payload, len) = next_capsule(frame.payload).unwrap();
        assert_eq!((ty, len), (CLOSE_WEBTRANSPORT_SESSION, frame.payload.len()));
        assert_eq!(&payload[..4], &1u32.to_be_bytes());
        assert_eq!(&payload[4..], b"bad_token");
        let mut long = Vec::new();
        put_close(&mut long, 0, &"é".repeat(600));
        let frame = next(&long).unwrap().unwrap();
        let (_, payload, _) = next_capsule(frame.payload).unwrap();
        assert_eq!(
            payload.len(),
            4 + 1024,
            "the reason is cut to 1024 bytes at a character boundary"
        );
    }

    #[test]
    fn an_oversized_frame_is_excessive_load() {
        let mut out = Vec::new();
        varint::put(&mut out, DATA);
        varint::put(&mut out, MOST_FRAME + 1);
        assert_eq!(next(&out).unwrap_err().code, code::H3_EXCESSIVE_LOAD);
    }
}
