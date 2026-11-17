//! The known attacks on an HTTP/2 endpoint, each driven past its cap with raw frames (or a real
//! peer's frames), each ending in its typed error at the cap and not one frame later:
//!
//! - rapid reset (CVE-2023-44487): `peer_resets` per `peer_resets_window`, then ENHANCE_YOUR_CALM;
//! - the CONTINUATION flood (2024): `continuations` per block and `header_block` bytes per block;
//! - the SETTINGS and PING floods (CVE-2019-9515, CVE-2019-9512): `pending_acks` acks owed;
//! - the zero-window stall (CVE-2019-9517's cousin): a stream stalled `stall_timeout` is reset;
//! - the unanswered SETTINGS: `settings_timeout`, then SETTINGS_TIMEOUT;
//! - the header list bomb (HPACK's amplification, CVE-2016-6581's kind): `header_list` decoded
//!   bytes, a stream error, and the HPACK state kept whole for the streams after;
//! - and the refusals: PUSH_PROMISE, ENABLE_PUSH to a client, an oversized frame, malformed
//!   fields going out and coming in.
//!
//! Each test prints cargo's `test tests::attacks::<name> ... ok`.

use core::time::Duration;
use std::{vec, vec::Vec as StdVec};

use blaze_own::AVec;

use super::{alloc, bytes, cat, hex};
use crate::{
    Connection, ErrorCode, Event, H2Error, HeadFault, HeadWhy, Instant, Limits, Refusal, ResetBy, Rule, StreamId,
    frame::{self, FrameKind, PREFACE, flags},
    head::{Field, RequestHead},
};

/// The connection error a call ended in, as (code, rule); anything else fails the test.
fn fault_of<T: core::fmt::Debug>(result: Result<T, H2Error>) -> (ErrorCode, Rule) {
    match result {
        Err(H2Error::Connection(fault)) => (fault.code, fault.rule),
        other => panic!("wanted a connection error, got {other:?}"),
    }
}

/// A frame header alone.
fn raw(len: u32, kind: FrameKind, flags: u8, stream: u32) -> StdVec<u8> {
    let mut out = AVec::new_in(alloc());
    frame::write_header(&mut out, len, kind, flags, StreamId(stream)).unwrap();
    bytes(&out)
}

/// A server that has read a client preface with empty SETTINGS (it owes one ack).
fn server_ready(limits: Limits) -> Connection<'static> {
    let mut server = Connection::server(limits, Instant::ZERO, alloc()).unwrap();
    server.received(&cat(&[PREFACE, &hex("000000 04 00 00000000")]), Instant::ZERO).unwrap();
    server
}

/// A client that has read a server's empty SETTINGS, with its own first bytes taken.
fn client_ready(limits: Limits) -> Connection<'static> {
    let mut client = Connection::client(limits, Instant::ZERO, alloc()).unwrap();
    client.received(&hex("000000 04 00 00000000"), Instant::ZERO).unwrap();
    drain(&mut client);
    client
}

fn drain(conn: &mut Connection<'_>) -> StdVec<u8> {
    let mut all = StdVec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = conn.transmit(&mut buf);
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

/// The resets told so far, other events dropped.
fn resets(conn: &mut Connection<'_>) -> StdVec<(StreamId, ErrorCode, ResetBy)> {
    let mut out = StdVec::new();
    while let Some(event) = conn.poll_event() {
        if let Event::Reset { stream, code, by } = event {
            out.push((stream, code, by));
        }
    }
    out
}

fn get(path: &'static [u8]) -> RequestHead<'static> {
    RequestHead {
        method: b"GET",
        scheme: Some(&b"https"[..]),
        authority: Some(&b"cache.example"[..]),
        path: Some(path),
        fields: &[],
    }
}

/// Rapid reset: a real client opens a stream and resets it, again and again, at one instant.
/// With a budget of 10 per window, the 10th is taken and the 11th ends the connection.
#[test]
fn rapid_reset_is_stopped_at_its_budget() {
    let limits = Limits { peer_resets: 10, ..Limits::DEFAULT };
    let mut server = Connection::server(limits, Instant::ZERO, alloc()).unwrap();
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    for k in 1..=11u32 {
        let id = client.request(&get(b"/"), false).unwrap();
        assert_eq!(id, StreamId(2 * k - 1));
        client.reset(id, ErrorCode::CANCEL).unwrap();
        let result = server.received(&drain(&mut client), Instant::ZERO);
        if k <= 10 {
            result.unwrap();
        } else {
            assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::RapidReset { most: 10 }));
        }
    }
    assert!(server.is_closed());
    // The GOAWAY names the code and the last stream the server saw.
    let out = drain(&mut server);
    assert!(out.ends_with(&hex("000008 07 00 00000000 00000015 0000000b")), "{out:02x?}");
}

/// The budget refills: one reset per `window / peer_resets` of time.
#[test]
fn rapid_reset_budget_refills_with_time() {
    let limits = Limits { peer_resets: 10, peer_resets_window: Duration::from_secs(10), ..Limits::DEFAULT };
    let mut server = Connection::server(limits, Instant::ZERO, alloc()).unwrap();
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    // Ten at once, then one a second for a minute: never over.
    for k in 0..70u64 {
        let id = client.request(&get(b"/"), false).unwrap();
        client.reset(id, ErrorCode::CANCEL).unwrap();
        let at = Instant::ZERO.after(Duration::from_secs(k.saturating_sub(9)));
        server.received(&drain(&mut client), at).unwrap();
    }
}

/// HEADERS without END_HEADERS, then CONTINUATIONs of 100 bytes: 32 are taken, the 33rd ends the
/// connection before its bytes are kept.
#[test]
fn a_continuation_flood_is_stopped_at_its_count() {
    let mut server = server_ready(Limits::DEFAULT);
    let piece = [0u8; 100];
    server.received(&cat(&[&raw(100, FrameKind::Headers, 0, 1), &piece]), Instant::ZERO).unwrap();
    for k in 1..=33u32 {
        let result = server.received(&cat(&[&raw(100, FrameKind::Continuation, 0, 1), &piece]), Instant::ZERO);
        if k <= 32 {
            result.unwrap();
        } else {
            assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::ContinuationsOverCap { most: 32 }));
        }
    }
}

/// Empty CONTINUATIONs grow no block, so only the count stops them.
#[test]
fn empty_continuations_are_stopped_at_the_same_count() {
    let mut server = server_ready(Limits::DEFAULT);
    server.received(&raw(0, FrameKind::Headers, 0, 1), Instant::ZERO).unwrap();
    for _ in 0..32 {
        server.received(&raw(0, FrameKind::Continuation, 0, 1), Instant::ZERO).unwrap();
    }
    let result = server.received(&raw(0, FrameKind::Continuation, 0, 1), Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::ContinuationsOverCap { most: 32 }));
}

/// Full-size CONTINUATIONs with the count cap out of the way: 4 x 16384 = 65536 bytes fit the
/// 64 KiB block cap exactly; the 4th CONTINUATION would pass it and ends the connection.
#[test]
fn a_continuation_flood_is_stopped_at_its_bytes() {
    let mut server = server_ready(Limits { continuations: 1000, ..Limits::DEFAULT });
    let piece = vec![0u8; 16_384];
    server.received(&cat(&[&raw(16_384, FrameKind::Headers, 0, 1), &piece]), Instant::ZERO).unwrap();
    for k in 1..=4 {
        let result = server.received(&cat(&[&raw(16_384, FrameKind::Continuation, 0, 1), &piece]), Instant::ZERO);
        if k <= 3 {
            result.unwrap();
        } else {
            assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::HeaderBlockOverCap { most: 65_536 }));
        }
    }
}

/// SETTINGS sent faster than the acks are read: the preface's SETTINGS and 15 more are owed; the
/// 17th is refused. Reading the acks out clears the count.
#[test]
fn a_settings_flood_is_stopped_at_the_acks_owed() {
    let mut server = server_ready(Limits::DEFAULT);
    let empty = hex("000000 04 00 00000000");
    for _ in 2..=16 {
        server.received(&empty, Instant::ZERO).unwrap();
    }
    let result = server.received(&empty, Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::AcksOverCap { most: 16 }));

    let mut server = server_ready(Limits::DEFAULT);
    for _ in 0..100 {
        server.received(&empty, Instant::ZERO).unwrap();
        drain(&mut server);
    }
}

/// PINGs on top of the preface's SETTINGS ack: 15 are answered, the 16th is refused.
#[test]
fn a_ping_flood_is_stopped_at_the_acks_owed() {
    let mut server = server_ready(Limits::DEFAULT);
    let ping = cat(&[&hex("000008 06 00 00000000"), b"floodfld"]);
    for _ in 1..=15 {
        server.received(&ping, Instant::ZERO).unwrap();
    }
    let result = server.received(&ping, Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::ENHANCE_YOUR_CALM, Rule::AcksOverCap { most: 16 }));
}

/// A peer that opens no window: the server's INITIAL_WINDOW_SIZE is 0, so a request body cannot
/// move. The stream is reset with CANCEL at `stall_timeout` (30 s), and only the RST is sent.
#[test]
fn a_zero_window_stall_is_reset_at_its_deadline() {
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    let server_says = cat(&[&hex("000006 04 00 00000000 0004 00000000"), &hex("000000 04 01 00000000")]);
    client.received(&server_says, Instant::ZERO).unwrap();
    let head = RequestHead { method: b"POST", ..get(b"/upload") };
    let id = client.request(&head, false).unwrap();
    assert_eq!(client.send_body(id, b"hello", true).unwrap(), 0);
    let deadline = Instant::ZERO.after(Duration::from_secs(30));
    assert_eq!(client.next_deadline(), Some(deadline));
    drain(&mut client);
    assert!(resets(&mut client).is_empty());

    client.timed_out(Instant::from_nanos(deadline.nanos() - 1)).unwrap();
    assert!(resets(&mut client).is_empty());
    client.timed_out(deadline).unwrap();
    let told = resets(&mut client);
    assert_eq!(told, [(id, ErrorCode::CANCEL, ResetBy::Us { frame: None, rule: Rule::SendStalled })]);
    assert_eq!(drain(&mut client), hex("000004 03 00 00000001 00000008"));
    assert_eq!(client.stream_state(id), None);
    assert_eq!(client.next_deadline(), None);
}

/// A peer that never acknowledges our SETTINGS: SETTINGS_TIMEOUT at `settings_timeout`.
#[test]
fn an_unanswered_settings_ends_the_connection_at_its_deadline() {
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    let deadline = Instant::ZERO.after(Duration::from_secs(10));
    assert_eq!(client.next_deadline(), Some(deadline));
    client.timed_out(Instant::from_nanos(deadline.nanos() - 1)).unwrap();
    let result = client.timed_out(deadline);
    assert_eq!(fault_of(result), (ErrorCode::SETTINGS_TIMEOUT, Rule::SettingsTimeout));
    assert!(drain(&mut client).ends_with(&hex("000008 07 00 00000000 00000000 00000004")));
}

/// A 5 KB block that decodes to 4 MB: one 4000-byte entry, then 1000 one-byte references to it.
/// The list passes 16 KiB, so the stream is reset with ENHANCE_YOUR_CALM; the decoder went on to
/// the end of the block, so the next response that names the entry decodes it.
#[test]
fn a_header_list_bomb_resets_its_stream_and_keeps_hpack_whole() {
    let mut client = client_ready(Limits { header_list: 16 << 10, ..Limits::DEFAULT });
    let first = client.request(&get(b"/a"), true).unwrap();
    let mut block = cat(&[&hex("40 06"), b"x-bomb", &hex("7f a1 1e")]);
    block.extend_from_slice(&[b'a'; 4000]);
    block.extend_from_slice(&[0xbe; 1000]);
    assert!(block.len() < 5100);
    let mut out = AVec::new_in(alloc());
    frame::write_headers(&mut out, first, &block, true, 16_384).unwrap();
    client.received(&bytes(&out), Instant::ZERO).unwrap();
    let rule = Rule::HeaderListOverCap { most: 16 << 10 };
    let by = ResetBy::Us { frame: Some(FrameKind::Headers), rule };
    assert_eq!(resets(&mut client), [(first, ErrorCode::ENHANCE_YOUR_CALM, by)]);
    assert_eq!(client.decoder().table().len(), 1);

    let second = client.request(&get(b"/b"), true).unwrap();
    let mut out = AVec::new_in(alloc());
    frame::write_headers(&mut out, second, &hex("88 be"), true, 16_384).unwrap();
    client.received(&bytes(&out), Instant::ZERO).unwrap();
    let mut seen = false;
    while let Some(event) = client.poll_event() {
        if let Event::Headers { stream, fields, end } = event {
            assert_eq!((stream, end), (second, true));
            assert_eq!(fields.status(), Some(200));
            assert_eq!(fields.value(b"x-bomb"), Some(&[b'a'; 4000][..]));
            seen = true;
        }
    }
    assert!(seen);
}

/// We send ENABLE_PUSH = 0, so a PUSH_PROMISE is a connection error (section 8.4).
#[test]
fn a_push_promise_is_refused() {
    let mut client = client_ready(Limits::DEFAULT);
    client.request(&get(b"/"), true).unwrap();
    let result = client.received(&hex("000004 05 04 00000001 00000002"), Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::PROTOCOL_ERROR, Rule::PushRefused));
}

/// A server may not send ENABLE_PUSH = 1 (section 6.5.2).
#[test]
fn enable_push_to_a_client_is_refused() {
    let mut client = Connection::client(Limits::DEFAULT, Instant::ZERO, alloc()).unwrap();
    let result = client.received(&hex("000006 04 00 00000000 0002 00000001"), Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::PROTOCOL_ERROR, Rule::SettingsValue { id: 2, value: 1 }));
}

/// A frame longer than our SETTINGS_MAX_FRAME_SIZE is FRAME_SIZE_ERROR (section 4.2), told from
/// its header alone.
#[test]
fn an_oversized_frame_is_refused() {
    let mut server = server_ready(Limits::DEFAULT);
    let result = server.received(&raw(16_385, FrameKind::Data, flags::END_STREAM, 1), Instant::ZERO);
    assert_eq!(fault_of(result), (ErrorCode::FRAME_SIZE_ERROR, Rule::FrameOverMax { len: 16_385, most: 16_384 }));
}

/// What we send is checked as what we receive (section 8.2): upper case, connection-specific
/// fields and `te` other than "trailers" are refused before a byte is written.
#[test]
fn malformed_fields_are_not_sent() {
    let mut client = client_ready(Limits::DEFAULT);
    let cases: [(Field<'static>, HeadWhy); 4] = [
        (Field::new(b"X-Up", b"v"), HeadWhy::NameUpperCase),
        (Field::new(b"connection", b"keep-alive"), HeadWhy::ConnectionSpecific),
        (Field::new(b"transfer-encoding", b"chunked"), HeadWhy::ConnectionSpecific),
        (Field::new(b"te", b"gzip"), HeadWhy::TeNotTrailers),
    ];
    for (field, why) in cases {
        let fields = [field];
        let head = RequestHead { fields: &fields, ..get(b"/") };
        match client.request(&head, true) {
            Err(H2Error::Refused(Refusal::Head(HeadFault { why: got, .. }))) => assert_eq!(got, why),
            other => panic!("wanted {why:?}, got {other:?}"),
        }
    }
    assert!(drain(&mut client).is_empty());
    assert_eq!(client.streams().count(), 0);
}

/// A response carrying `connection: close` is malformed (section 8.2.2): a stream error, the
/// connection goes on.
#[test]
fn a_malformed_response_resets_its_stream() {
    let mut client = client_ready(Limits::DEFAULT);
    let id = client.request(&get(b"/"), true).unwrap();
    // The request's own HEADERS frame is not what this test is about.
    drain(&mut client);
    let block = cat(&[&hex("88 00 0a"), b"connection", &hex("05"), b"close"]);
    let mut out = AVec::new_in(alloc());
    frame::write_headers(&mut out, id, &block, true, 16_384).unwrap();
    client.received(&bytes(&out), Instant::ZERO).unwrap();
    let rule = Rule::Malformed(HeadWhy::ConnectionSpecific);
    let by = ResetBy::Us { frame: Some(FrameKind::Headers), rule };
    assert_eq!(resets(&mut client), [(id, ErrorCode::PROTOCOL_ERROR, by)]);
    assert!(!client.is_closed());
    assert_eq!(drain(&mut client), hex("000004 03 00 00000001 00000001"));
}
