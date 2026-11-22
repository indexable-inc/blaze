//! A client and a server, wired back to back, with generated requests (methods, bodies,
//! `content-length` or not), response bodies, windows and frame sizes on both sides, and the
//! bytes of each direction delivered in generated pieces (every split point is a case: a piece
//! is one to sixteen bytes a quarter of the time). The oracle: every body byte arrives exactly
//! once and in order, both ways; no stream is reset; both tables end empty (every stream reached
//! section 5.1's closed state and its body was read); after every round no flow-control window
//! leaves 0..=2^31-1 and no buffer passes its cap; the exchange never stalls.
//!
//! Prints blaze-sim's one pass line for `back_to_back_delivers_every_byte_once`, and for the
//! control `the_oracle_catches_a_window_update_on_the_wrong_stream` the plant's line ("plant
//! WindowUpdateWrongStream: caught at case k of n"). The plant (conn.rs
//! `Connection::on_window_update`): a WINDOW_UPDATE for a stream is applied to the stream before
//! it in the table, so the sender overruns one stream's window and starves the other; the oracle
//! says "exchange broke" (a stream reset with FLOW_CONTROL_ERROR, or a stall).

use std::{format, string::String, vec, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape, check, control};

use super::alloc;
use crate::{
    Connection, Event, H2Error, Instant, Limits, Refusal, StreamId,
    frame::MAX_31,
    head::{Field, RequestHead, ResponseHead},
    plant::{self, Plant},
};

struct Req {
    post: bool,
    path: StdVec<u8>,
    body: StdVec<u8>,
    body_len: String,
    response: StdVec<u8>,
    response_len: String,
    length_field: bool,
}

#[derive(Default)]
struct ClientSide {
    id: Option<StreamId>,
    sent: usize,
    status: Option<u16>,
    got: StdVec<u8>,
    ended: bool,
}

struct ServerSide {
    id: StreamId,
    req: usize,
    body: StdVec<u8>,
    req_ended: bool,
    responded: bool,
    sent: usize,
}

fn broke(what: &str, e: H2Error) -> Broke {
    Broke::Oracle(format!("exchange broke: {what}: {e}"))
}

/// A body of `len` bytes that differs by request and by position.
fn pattern(k: usize, len: usize, salt: usize) -> StdVec<u8> {
    (0..len).map(|i| u8::try_from((i * 31 + k * 7 + salt) % 251).unwrap_or(0)).collect()
}

fn draw_limits(tape: &Tape) -> Limits {
    Limits {
        stream_window: tape.int_in(65_535u32..=300_000, "stream window"),
        conn_window: tape.int_in(65_535u32..=1_000_000, "conn window"),
        frame_size: *tape.pick(&[16_384u32, 20_000, 65_536], "frame size").unwrap_or(&16_384),
        ..Limits::DEFAULT
    }
}

/// Moves everything `from` has to send into `to`, in drawn pieces.
fn pump(from: &mut Connection<'_>, to: &mut Connection<'_>, tape: &Tape, buf: &mut [u8], now: Instant, what: &str) -> Result<bool, Broke> {
    let mut moved = false;
    loop {
        let n = from.transmit(buf);
        if n == 0 {
            return Ok(moved);
        }
        moved = true;
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            let size = if tape.one_in(4, "tiny piece") {
                tape.int_in(1usize..=16, "piece")
            } else {
                tape.int_in(1usize..=40_000, "piece")
            };
            let (piece, after) = rest.split_at(size.min(rest.len()));
            to.received(piece, now).map_err(|e| broke(what, e))?;
            rest = after;
        }
    }
}

/// Reads up to a drawn budget of `id`'s body into `into`; a stream gone (closed and read) reads 0.
fn read_some(conn: &mut Connection<'_>, id: StreamId, into: &mut StdVec<u8>, tape: &Tape, buf: &mut [u8]) -> Result<bool, Broke> {
    let budget = tape.int_in(1usize..=70_000, "read budget");
    let mut total = 0;
    while total < budget {
        let want = (budget - total).min(buf.len());
        match conn.read(id, &mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                into.extend_from_slice(&buf[..n]);
                total += n;
            }
            Err(H2Error::Refused(Refusal::NoSuchStream { .. })) => break,
            Err(e) => return Err(broke("read", e)),
        }
    }
    Ok(total > 0)
}

/// No window outside 0..=2^31-1, no buffer past its cap.
fn windows_ok(conn: &Connection<'_>, limits: &Limits, who: &str) -> Result<(), Broke> {
    let range = 0..=i64::from(MAX_31);
    let w = conn.windows();
    if !range.contains(&w.conn_send) || !range.contains(&w.conn_recv) {
        return Err(Broke::Oracle(format!("exchange broke: {who}'s connection windows {w:?}")));
    }
    for id in conn.streams() {
        if let Some((send, recv)) = conn.stream_windows(id)
            && (!range.contains(&send) || !range.contains(&recv))
        {
            return Err(Broke::Oracle(format!("exchange broke: {who}'s stream {} windows {send}, {recv}", id.0)));
        }
    }
    if conn.buffered() > usize::try_from(limits.conn_window).unwrap_or(usize::MAX) {
        return Err(Broke::Oracle(format!("exchange broke: {who} buffers {} bytes", conn.buffered())));
    }
    Ok(())
}

fn exchange(tape: &Tape) -> Result<(), Broke> {
    let client_limits = draw_limits(tape);
    let server_limits = draw_limits(tape);
    let harness = |e: H2Error| Broke::Harness(format!("{e}"));
    let mut client = Connection::client(client_limits, Instant::ZERO, alloc()).map_err(harness)?;
    let mut server = Connection::server(server_limits, Instant::ZERO, alloc()).map_err(harness)?;
    let count = tape.int_in(1usize..=6, "requests");
    let reqs: StdVec<Req> = (0..count)
        .map(|k| {
            let post = tape.coin("post");
            let body = if post { pattern(k, tape.int_in(0usize..=100_000, "request body"), 1) } else { StdVec::new() };
            let response = pattern(k, tape.int_in(0usize..=100_000, "response body"), 2);
            Req {
                post,
                path: format!("/r/{k}").into_bytes(),
                body_len: format!("{}", body.len()),
                response_len: format!("{}", response.len()),
                body,
                response,
                length_field: tape.coin("content-length"),
            }
        })
        .collect();
    let mut cs: StdVec<ClientSide> = (0..count).map(|_| ClientSide::default()).collect();
    let mut ss: StdVec<ServerSide> = StdVec::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut idle_rounds = 0;
    for round in 0..20_000u64 {
        let now = Instant::from_nanos(round * 1_000_000);
        let mut progress = false;

        for (k, req) in reqs.iter().enumerate() {
            let c = &mut cs[k];
            if c.id.is_none() {
                let length = [Field::new(b"content-length", req.body_len.as_bytes()), Field::new(b"x-k", b"v")];
                let fields: &[Field<'_>] = if req.length_field { &length } else { &length[1..] };
                let head = RequestHead {
                    method: if req.post { &b"POST"[..] } else { &b"GET"[..] },
                    scheme: Some(&b"https"[..]),
                    authority: Some(&b"cache.example"[..]),
                    path: Some(req.path.as_slice()),
                    fields,
                };
                c.id = Some(client.request(&head, req.body.is_empty()).map_err(|e| broke("request", e))?);
                progress = true;
            }
            if let Some(id) = c.id
                && c.sent < req.body.len()
            {
                let n = client.send_body(id, &req.body[c.sent..], true).map_err(|e| broke("client send_body", e))?;
                c.sent += n;
                progress |= n > 0;
            }
        }
        progress |= pump(&mut client, &mut server, tape, &mut buf, now, "server received")?;

        while let Some(event) = server.poll_event() {
            progress = true;
            match event {
                Event::Headers { stream, fields, end } => {
                    let path = fields.value(b":path").map(<[u8]>::to_vec).unwrap_or_default();
                    let Some(req) = reqs.iter().position(|r| r.path == path) else {
                        return Err(Broke::Oracle(format!("exchange broke: a request for an unknown path on {}", stream.0)));
                    };
                    ss.push(ServerSide { id: stream, req, body: StdVec::new(), req_ended: end, responded: false, sent: 0 });
                }
                Event::End { stream } => {
                    if let Some(s) = ss.iter_mut().find(|s| s.id == stream) {
                        s.req_ended = true;
                    }
                }
                Event::Reset { stream, code, by } => {
                    return Err(Broke::Oracle(format!("exchange broke: the server saw stream {} reset {code:?} by {by:?}", stream.0)));
                }
                _ => {}
            }
        }
        for s in &mut ss {
            progress |= read_some(&mut server, s.id, &mut s.body, tape, &mut buf)?;
            let req = &reqs[s.req];
            if s.req_ended && !s.responded {
                let length = [Field::new(b"content-length", req.response_len.as_bytes())];
                let fields: &[Field<'_>] = if req.length_field { &length } else { &[] };
                server
                    .respond(s.id, &ResponseHead { status: 200, fields }, req.response.is_empty())
                    .map_err(|e| broke("respond", e))?;
                s.responded = true;
                progress = true;
            }
            if s.responded && s.sent < req.response.len() {
                let n = server.send_body(s.id, &req.response[s.sent..], true).map_err(|e| broke("server send_body", e))?;
                s.sent += n;
                progress |= n > 0;
            }
        }
        progress |= pump(&mut server, &mut client, tape, &mut buf, now, "client received")?;

        while let Some(event) = client.poll_event() {
            progress = true;
            match event {
                Event::Headers { stream, fields, .. } => {
                    if let Some(c) = cs.iter_mut().find(|c| c.id == Some(stream)) {
                        c.status = fields.status();
                    }
                }
                Event::End { stream } => {
                    if let Some(c) = cs.iter_mut().find(|c| c.id == Some(stream)) {
                        c.ended = true;
                    }
                }
                Event::Reset { stream, code, by } => {
                    return Err(Broke::Oracle(format!("exchange broke: the client saw stream {} reset {code:?} by {by:?}", stream.0)));
                }
                _ => {}
            }
        }
        for c in &mut cs {
            if let Some(id) = c.id {
                progress |= read_some(&mut client, id, &mut c.got, tape, &mut buf)?;
            }
        }
        windows_ok(&client, &client_limits, "the client")?;
        windows_ok(&server, &server_limits, "the server")?;

        let done = cs.iter().zip(&reqs).all(|(c, r)| c.ended && c.got.len() == r.response.len())
            && client.streams().next().is_none()
            && server.streams().next().is_none();
        if done {
            for (k, (c, r)) in cs.iter().zip(&reqs).enumerate() {
                if c.status != Some(200) || c.got != r.response {
                    return Err(Broke::Oracle(format!("exchange broke: response {k} differs (status {:?})", c.status)));
                }
                let Some(s) = ss.iter().find(|s| s.req == k) else {
                    return Err(Broke::Oracle(format!("exchange broke: request {k} never reached the server")));
                };
                if s.body != r.body {
                    return Err(Broke::Oracle(format!("exchange broke: request body {k} differs: {} of {} bytes", s.body.len(), r.body.len())));
                }
            }
            return Ok(());
        }
        idle_rounds = if progress { 0 } else { idle_rounds + 1 };
        if idle_rounds >= 3 {
            return Err(Broke::Oracle(format!("exchange broke: stalled at round {round}")));
        }
    }
    Err(Broke::Oracle(String::from("exchange broke: not done in 20000 rounds")))
}

/// The invariant: back-to-back HTTP/2 connections deliver every request and response body byte exactly once and in order under arbitrary framing and chunk splits while keeping all flow-control windows valid.
#[test]
fn back_to_back_delivers_every_byte_once() {
    check(Config::cases(32), exchange);
}

#[test]
fn the_oracle_catches_a_window_update_on_the_wrong_stream() {
    control(Config::cases(48), Plant::Clean, Plant::WindowUpdateWrongStream, "exchange broke", |tape, p| {
        plant::with(p, || exchange(tape))
    });
}
