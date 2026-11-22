//! The frame layer and the stream table. RFC 9113 prints no frame in hex, so the expected bytes
//! here are built from its layouts (section 4.1's header, section 6's payloads), field by field,
//! and the stream table from section 5.1's figure and prose.
//!
//! Each test prints cargo's `test tests::frames::<name> ... ok`.

use std::vec::Vec as StdVec;

use blaze_own::AVec;

use super::{alloc, bytes, cat, hex};
use crate::{
    Connection, Instant, Limits,
    frame::{self, FrameKind, Header, PREFACE, Priority, StreamId, flags},
    stream::{self, STATES, STEPS, State, Step},
};

/// Section 4.1: a 24-bit length, the type, the flags, the reserved bit (ignored on receipt) and
/// a 31-bit stream identifier.
#[test]
fn the_frame_header_is_section_4_1() {
    let h = frame::parse_header(&[0x00, 0x00, 0x08, 0x06, 0x01, 0x80, 0x00, 0x00, 0x00]);
    assert_eq!(h, Header { len: 8, kind: FrameKind::Ping, flags: flags::ACK, stream: StreamId(0) });
    let h = frame::parse_header(&[0x01, 0x02, 0x03, 0x09, 0x04, 0x7f, 0xff, 0xff, 0xff]);
    assert_eq!(h, Header { len: 0x010203, kind: FrameKind::Continuation, flags: flags::END_HEADERS, stream: StreamId(0x7fff_ffff) });
    let h = frame::parse_header(&[0, 0, 0, 0xfe, 0, 0, 0, 0, 3]);
    assert_eq!(h.kind, FrameKind::Unknown(0xfe));
    let mut out = AVec::new_in(alloc());
    frame::write_header(&mut out, 0x010203, FrameKind::Continuation, flags::END_HEADERS, StreamId(0x7fff_ffff)).unwrap();
    assert_eq!(bytes(&out), hex("010203 09 04 7fffffff"));
}

/// Every frame we write, from its section's layout.
#[test]
fn frames_are_written_as_section_6_lays_them_out() {
    let mut out = AVec::new_in(alloc());
    frame::write_rst(&mut out, StreamId(1), crate::ErrorCode::CANCEL).unwrap();
    frame::write_window_update(&mut out, StreamId(0), 0x3f_0001).unwrap();
    frame::write_ping(&mut out, *b"12345678", true).unwrap();
    frame::write_goaway(&mut out, StreamId(5), crate::ErrorCode::ENHANCE_YOUR_CALM).unwrap();
    frame::write_settings_ack(&mut out).unwrap();
    frame::write_priority(&mut out, StreamId(3), Priority { exclusive: true, depends_on: StreamId(1), weight: 15 }).unwrap();
    frame::write_data(&mut out, StreamId(3), b"hi", true).unwrap();
    let want = cat(&[
        &hex("000004 03 00 00000001 00000008"),
        &hex("000004 08 00 00000000 003f0001"),
        &hex("000008 06 01 00000000"),
        b"12345678",
        &hex("000008 07 00 00000000 00000005 0000000b"),
        &hex("000000 04 01 00000000"),
        &hex("000005 02 00 00000003 80000001 0f"),
        &hex("000002 00 01 00000003"),
        b"hi",
    ]);
    assert_eq!(bytes(&out), want);
}

/// A field block longer than the peer's frame size is HEADERS then CONTINUATIONs, END_STREAM on
/// the HEADERS only, END_HEADERS on the last only (sections 4.3, 6.2, 6.10).
#[test]
fn a_long_block_is_split_into_continuations() {
    let block: StdVec<u8> = (0..40u8).collect();
    let mut out = AVec::new_in(alloc());
    frame::write_headers(&mut out, StreamId(1), &block, true, 16).unwrap();
    let want = cat(&[
        &hex("000010 01 01 00000001"),
        &block[..16],
        &hex("000010 09 00 00000001"),
        &block[16..32],
        &hex("000008 09 04 00000001"),
        &block[32..],
    ]);
    assert_eq!(bytes(&out), want);
}

/// Padding (section 6.1): stripped; a pad length not less than the payload is refused.
#[test]
fn padding_is_stripped_and_checked() {
    let h = |len| Header { len, kind: FrameKind::Data, flags: flags::PADDED, stream: StreamId(1) };
    assert_eq!(frame::unpad(h(6), &[2, b'a', b'b', b'c', 0, 0]), Ok(&b"abc"[..]));
    assert!(frame::unpad(h(3), &[3, 0, 0]).is_err());
    assert!(frame::unpad(h(0), &[]).is_err());
}

/// The client's first bytes, exactly: the preface, SETTINGS from `Limits::DEFAULT` (header
/// table 4096, push 0, 128 streams, a 256 KiB window, 16384-byte frames, a 64 KiB list), and
/// the connection WINDOW_UPDATE from 65,535 to 4 MiB (an increment of 0x3f0001). The same on
/// every machine.
#[test]
fn the_client_preface_is_exact() {
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    let mut out = [0u8; 256];
    let n = client.transmit(&mut out);
    let want = cat(&[
        PREFACE,
        &hex("000024 04 00 00000000"),
        &hex("0001 00001000 0002 00000000 0003 00000080 0004 00040000 0005 00004000 0006 00010000"),
        &hex("000004 08 00 00000000 003f0001"),
    ]);
    assert_eq!(&out[..n], &want[..]);
}

/// Section 5.1's figure as edges; every other (state, step) pair has no edge.
#[test]
fn the_stream_table_is_section_5_1() {
    use State::*;
    use Step::*;
    let edges: &[(State, Step, State)] = &[
        (Idle, SendHeaders, Open),
        (Idle, RecvHeaders, Open),
        (Idle, SendPushPromise, ReservedLocal),
        (Idle, RecvPushPromise, ReservedRemote),
        (ReservedLocal, SendHeaders, HalfClosedRemote),
        (ReservedLocal, SendReset, Closed),
        (ReservedLocal, RecvReset, Closed),
        (ReservedRemote, RecvHeaders, HalfClosedLocal),
        (ReservedRemote, SendReset, Closed),
        (ReservedRemote, RecvReset, Closed),
        (Open, SendEnd, HalfClosedLocal),
        (Open, RecvEnd, HalfClosedRemote),
        (Open, SendReset, Closed),
        (Open, RecvReset, Closed),
        (Open, SendHeaders, Open),
        (Open, RecvHeaders, Open),
        (HalfClosedLocal, RecvEnd, Closed),
        (HalfClosedLocal, SendReset, Closed),
        (HalfClosedLocal, RecvReset, Closed),
        (HalfClosedLocal, RecvHeaders, HalfClosedLocal),
        (HalfClosedRemote, SendEnd, Closed),
        (HalfClosedRemote, SendReset, Closed),
        (HalfClosedRemote, RecvReset, Closed),
        (HalfClosedRemote, SendHeaders, HalfClosedRemote),
    ];
    for from in STATES {
        for step in STEPS {
            let want = edges.iter().find(|(f, s, _)| *f == from && *s == step).map(|e| e.2);
            assert_eq!(stream::next(from, step), want, "{from:?} on {step:?}");
        }
    }
    // Who may send: exactly the states with a send edge for H or ES.
    for state in STATES {
        let by_table = stream::next(state, SendEnd).is_some() || (state == ReservedLocal);
        assert_eq!(stream::sends(state), by_table, "{state:?}");
        let by_table = stream::next(state, RecvEnd).is_some() || (state == ReservedRemote);
        assert_eq!(stream::receives(state), by_table, "{state:?}");
    }
}
