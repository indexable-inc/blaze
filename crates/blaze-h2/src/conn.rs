//! The connection (RFC 9113): one sans-IO machine for both roles. Bytes in
//! ([`Connection::received`]), bytes out ([`Connection::transmit`]), events out
//! ([`Connection::poll_event`]), time in (every entry point that can pass a deadline takes `now`;
//! [`Connection::next_deadline`] says when to call [`Connection::timed_out`]).
//!
//! **Preface** (section 3.4): "the client connection preface starts with a sequence of 24
//! octets ... followed by a SETTINGS frame ... The server connection preface consists of a
//! potentially empty SETTINGS frame that MUST be the first frame the server sends". Each side's
//! first frame must be SETTINGS; ours is made from [`Limits`], with SETTINGS_ENABLE_PUSH = 0, and
//! is followed by a WINDOW_UPDATE that raises the connection window from 65,535 to
//! [`Limits::conn_window`]. "Clients are permitted to send additional frames to the server
//! immediately after sending the client connection preface, without waiting to receive the
//! server connection preface."
//!
//! **SETTINGS** (section 6.5.3): the peer's values apply when received, and are acknowledged at
//! once; ours apply to the peer once it acknowledges them, and a deadline
//! ([`Limits::settings_timeout`]) ends the connection with SETTINGS_TIMEOUT when it does not.
//!
//! **Flow control** (sections 5.2, 6.9): both directions, per connection and per stream. Sending,
//! [`Connection::send_body`] takes what the smaller window allows and says how much. Receiving,
//! the peer's DATA is buffered within the window we gave; the window is given back only as the
//! caller reads ([`Connection::read`]), by one stated policy: a WINDOW_UPDATE for a stream, or
//! for the connection, once half its window has been read since the last one. So the buffered
//! bytes of a stream never pass [`Limits::stream_window`], nor those of the connection
//! [`Limits::conn_window`]: a slow reader is back-pressure, never memory. Section 6.9.1: "A sender
//! MUST NOT allow a flow-control window to exceed 2^31-1 octets"; section 6.9.2: a change of
//! SETTINGS_INITIAL_WINDOW_SIZE moves every stream's send window by the difference, which may
//! make it negative.
//!
//! **Streams**: the table holds the streams not yet closed, and closed ones whose body the
//! caller has not read; sorted by identifier, so every walk is in one order on every machine.
//! States move only through [`crate::stream::next`].
//!
//! **Caps** (rule 18, section 10.5): every count and buffer a peer can grow has a cap in
//! [`Limits`]; passing one is a typed stream or connection error with the RFC's code, listed on
//! each field of [`Limits`].

#![blaze::sans_io]

use blaze_own::{ADeque, AVec, Alloc, VecDeque};

use crate::{
    error::{ErrorCode, Fault, H2Error, Refusal, Rule},
    frame::{self, FrameKind, HEADER_LEN, Header, MAX_31, PREFACE, StreamId, flags},
    head::{self, Field, Fields, RequestHead, ResponseHead},
    hpack::{Decoder, Encoder, Halt, table::entry_size},
    plant::{self, Plant},
    settings::{DEFAULT_WINDOW, Limits, Settings},
    stream::{self, State, Step},
    time::Instant,
};

/// Which end of the connection this machine is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Client,
    Server,
}

/// Who reset a stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetBy {
    /// The peer sent RST_STREAM.
    Peer,
    /// We did, for the peer's fault (`frame` is none for a deadline).
    Us { frame: Option<FrameKind>, rule: Rule },
    /// The peer's GOAWAY: the stream is above its last stream identifier and was not processed
    /// (section 6.8: such requests may be retried).
    GoAway,
}

/// What happened, for the caller.
pub enum Event<'a> {
    /// The peer's SETTINGS took effect (its first, then each change; told once per poll).
    Settings(Settings),
    /// The peer acknowledged our SETTINGS.
    SettingsAcked,
    /// A head: a request (server role), an informational or final response (client role), or
    /// trailers. `end`: the peer ended the stream with it (an [`Event::End`] follows).
    Headers { stream: StreamId, fields: Fields<'a>, end: bool },
    /// Body bytes are buffered for [`Connection::read`] (told once until the caller reads).
    Data { stream: StreamId },
    /// The peer ended the stream: what is buffered is the rest of the body.
    End { stream: StreamId },
    /// The stream was reset.
    Reset { stream: StreamId, code: ErrorCode, by: ResetBy },
    /// A stream that had more to send than its window allowed has window again.
    Writable { stream: StreamId },
    /// The peer is going away: no new stream; streams up to `last` go on.
    GoAway { last: StreamId, code: ErrorCode },
    /// A PING of ours was answered.
    Pong { data: [u8; 8] },
}

/// The connection's flow-control windows, for a driver's view and the tests' oracle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Windows {
    /// What we may still send on the connection.
    pub conn_send: i64,
    /// What the peer may still send on the connection.
    pub conn_recv: i64,
}

struct Stream<'a> {
    id: StreamId,
    state: State,
    /// Opened by us.
    ours: bool,
    send_window: i64,
    recv_window: i64,
    /// Bytes read by the caller (or dropped) since the last WINDOW_UPDATE for the stream.
    recv_unacked: u32,
    /// Body bytes received and not read, from `buf_at`.
    buf: AVec<'a, u8>,
    buf_at: usize,
    /// The body's length the head said (0 for a response to HEAD, a 204 or a 304).
    content_length: Option<u64>,
    received: u64,
    /// Client: our request was HEAD.
    head_method: bool,
    /// The peer's final head (client: the final response; server: the request) came.
    final_head: bool,
    /// Our head (server: the response) was sent.
    head_sent: bool,
    informational: u8,
    data_told: bool,
    /// When `send_body` last took less than it was given for want of window.
    blocked_since: Option<Instant>,
}

impl Stream<'_> {
    fn buffered(&self) -> usize {
        self.buf.len() - self.buf_at
    }
}

/// One HTTP/2 connection's machine.
pub struct Connection<'a> {
    role: Role,
    limits: Limits,
    alloc: Alloc<'a>,
    now: Instant,

    /// Server: bytes of the client's preface still to match.
    preface_left: usize,
    /// The peer's first SETTINGS came.
    got_settings: bool,
    /// A frame begun in an earlier `received`: at most a header and one frame's payload.
    inbuf: AVec<'a, u8>,

    /// The field block being gathered (HEADERS then CONTINUATIONs).
    block: AVec<'a, u8>,
    block_stream: Option<StreamId>,
    block_end_stream: bool,
    block_frames: u16,
    /// A stream error found in the block's first frame, applied once HPACK has taken the block.
    block_error: Option<(ErrorCode, Rule)>,
    decoder: Decoder<'a>,
    encoder: Encoder<'a>,
    scratch: AVec<'a, u8>,

    peer: Settings,
    /// When our SETTINGS went out, while unacknowledged.
    settings_sent_at: Option<Instant>,

    conn_send: i64,
    conn_recv: i64,
    conn_unacked: u32,

    streams: AVec<'a, Stream<'a>>,
    /// Our next stream's identifier.
    next_id: u32,
    /// The highest stream identifier the peer opened.
    highest_peer: u32,

    goaway_sent: Option<StreamId>,
    goaway_received: Option<StreamId>,
    closed: bool,
    fault: Option<Fault>,

    out: AVec<'a, u8>,
    out_at: usize,
    acks_pending: u16,
    resets_pending: u16,
    pings_out: u16,
    /// The rapid-reset budget, in units where one reset costs the window's nanoseconds.
    reset_tokens: u64,
    reset_refilled: Instant,

    events: ADeque<'a, Event<'a>>,
    settings_told: bool,
}

fn conn_fault(code: ErrorCode, frame: Option<FrameKind>, stream: StreamId, rule: Rule) -> H2Error {
    H2Error::Connection(Fault { code, frame, stream, rule })
}

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl<'a> Connection<'a> {
    /// A client's machine: its preface, SETTINGS and connection WINDOW_UPDATE are queued for
    /// [`Connection::transmit`].
    pub fn client(limits: Limits, now: Instant, alloc: Alloc<'a>) -> Result<Connection<'a>, H2Error> {
        Connection::new(Role::Client, limits, now, alloc)
    }

    /// A server's machine: it expects the client's preface first; its SETTINGS and connection
    /// WINDOW_UPDATE are queued for [`Connection::transmit`].
    pub fn server(limits: Limits, now: Instant, alloc: Alloc<'a>) -> Result<Connection<'a>, H2Error> {
        Connection::new(Role::Server, limits, now, alloc)
    }

    fn new(role: Role, limits: Limits, now: Instant, alloc: Alloc<'a>) -> Result<Connection<'a>, H2Error> {
        limits.check().map_err(Refusal::Limits)?;
        let reset_tokens = u64::from(limits.peer_resets) * window_nanos(&limits);
        let mut conn = Connection {
            role,
            limits,
            alloc,
            now,
            preface_left: if role == Role::Server { PREFACE.len() } else { 0 },
            got_settings: false,
            inbuf: AVec::new_in(alloc),
            block: AVec::new_in(alloc),
            block_stream: None,
            block_end_stream: false,
            block_frames: 0,
            block_error: None,
            // The peer's encoder may use the RFC's 4096 until it has our SETTINGS.
            decoder: Decoder::new(crate::settings::DEFAULT_TABLE, alloc),
            encoder: Encoder::new(limits.hpack_encoder_table, limits.strings, alloc),
            scratch: AVec::new_in(alloc),
            peer: Settings::INITIAL,
            settings_sent_at: Some(now),
            conn_send: i64::from(DEFAULT_WINDOW),
            conn_recv: i64::from(limits.conn_window),
            conn_unacked: 0,
            streams: AVec::new_in(alloc),
            next_id: if role == Role::Client { 1 } else { 2 },
            highest_peer: 0,
            goaway_sent: None,
            goaway_received: None,
            closed: false,
            fault: None,
            out: AVec::new_in(alloc),
            out_at: 0,
            acks_pending: 0,
            resets_pending: 0,
            pings_out: 0,
            reset_tokens,
            reset_refilled: now,
            events: VecDeque::new_in(alloc),
            settings_told: false,
        };
        if role == Role::Client {
            conn.out.try_extend_from_slice(PREFACE)?;
        }
        frame::write_settings(&mut conn.out, &limits.entries())?;
        let raise = limits.conn_window - DEFAULT_WINDOW;
        if raise > 0 {
            frame::write_window_update(&mut conn.out, StreamId::ZERO, raise)?;
        }
        Ok(conn)
    }

    // ---- What the caller reads. ----

    /// Which end this is.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The peer's settings in force.
    pub fn peer_settings(&self) -> Settings {
        self.peer
    }

    /// The fault that ended the connection, if one did.
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Whether the connection takes nothing more.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether a GOAWAY went either way and no stream is left: the driver may close the socket
    /// once [`Connection::pending_out`] is 0.
    pub fn is_done(&self) -> bool {
        self.closed || ((self.goaway_sent.is_some() || self.goaway_received.is_some()) && self.streams.is_empty())
    }

    /// A stream's state; None once it is closed and its body read (or never opened).
    pub fn stream_state(&self, id: StreamId) -> Option<State> {
        self.find(id).map(|i| self.streams.as_slice()[i].state)
    }

    /// A stream's (send, receive) windows.
    pub fn stream_windows(&self, id: StreamId) -> Option<(i64, i64)> {
        self.find(id).map(|i| {
            let s = &self.streams.as_slice()[i];
            (s.send_window, s.recv_window)
        })
    }

    /// The connection's windows.
    pub fn windows(&self) -> Windows {
        Windows { conn_send: self.conn_send, conn_recv: self.conn_recv }
    }

    /// The streams in the table, in identifier order.
    pub fn streams(&self) -> impl Iterator<Item = StreamId> + '_ {
        self.streams.iter().map(|s| s.id)
    }

    /// Body bytes buffered in all.
    pub fn buffered(&self) -> usize {
        self.streams.iter().map(Stream::buffered).sum()
    }

    /// Bytes waiting for [`Connection::transmit`].
    pub fn pending_out(&self) -> usize {
        self.out.len() - self.out_at
    }

    /// The HPACK decoder (the tests compare its table with the peer encoder's).
    pub fn decoder(&self) -> &Decoder<'a> {
        &self.decoder
    }

    /// The HPACK encoder.
    pub fn encoder(&self) -> &Encoder<'a> {
        &self.encoder
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<Event<'a>> {
        let event = self.events.pop_front()?;
        if matches!(event, Event::Settings(_)) {
            self.settings_told = false;
        }
        Some(event)
    }

    /// Moves up to `out.len()` bytes of what is to be sent into `out`, and says how many.
    pub fn transmit(&mut self, out: &mut [u8]) -> usize {
        let pending = &self.out.as_slice()[self.out_at..];
        let n = pending.len().min(out.len());
        out[..n].copy_from_slice(&pending[..n]);
        self.out_at += n;
        if self.out_at == self.out.len() {
            self.out.clear();
            self.out_at = 0;
            self.acks_pending = 0;
            self.resets_pending = 0;
        } else if self.out_at >= 64 << 10 && self.out_at * 2 >= self.out.len() {
            self.out.drain(..self.out_at);
            self.out_at = 0;
        }
        n
    }

    /// When [`Connection::timed_out`] must next be called, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        let settings = self.settings_sent_at.map(|at| at.after(self.limits.settings_timeout));
        let stalls = self.streams.iter().filter_map(|s| s.blocked_since).map(|at| at.after(self.limits.stall_timeout));
        settings.into_iter().chain(stalls).min()
    }

    // ---- What the caller asks. ----

    /// Opens a stream with `head` (client role); `end`: no body follows.
    pub fn request(&mut self, head: &RequestHead<'_>, end: bool) -> Result<StreamId, H2Error> {
        self.open_for_calls()?;
        if self.role != Role::Client {
            return Err(Refusal::WrongRole { needs: Role::Client }.into());
        }
        if self.goaway_sent.is_some() || self.goaway_received.is_some() {
            return Err(Refusal::GoingAway.into());
        }
        head::check_outgoing_request(head).map_err(Refusal::Head)?;
        if let Some(most) = self.peer.max_concurrent_streams {
            let open = self.streams.iter().filter(|s| s.ours && s.state != State::Closed).count();
            if open >= usize::try_from(most).unwrap_or(usize::MAX) {
                return Err(Refusal::StreamsFull { most }.into());
            }
        }
        if self.next_id > MAX_31 {
            return Err(Refusal::IdsExhausted.into());
        }
        let pseudo = [
            Some(Field::new(b":method", head.method)),
            head.scheme.map(|v| Field::new(b":scheme", v)),
            head.authority.map(|v| Field::new(b":authority", v)),
            head.path.map(|v| Field::new(b":path", v)),
        ];
        let fields = || pseudo.iter().flatten().copied().chain(head.fields.iter().copied());
        self.check_peer_list(fields())?;
        let id = StreamId(self.next_id);
        let mut stream = self.new_stream(id, true);
        stream.head_method = head.method == b"HEAD";
        stream.head_sent = true;
        stream.state = State::Open;
        if end {
            stream.state = State::HalfClosedLocal;
        }
        self.streams.try_reserve(1)?;
        self.send_block(id, fields(), end)?;
        self.next_id += 2;
        self.insert(stream);
        Ok(id)
    }

    /// Sends the response head on `stream` (server role); `end`: no body follows.
    pub fn respond(&mut self, id: StreamId, head: &ResponseHead<'_>, end: bool) -> Result<(), H2Error> {
        self.open_for_calls()?;
        if self.role != Role::Server {
            return Err(Refusal::WrongRole { needs: Role::Server }.into());
        }
        let i = self.find(id).ok_or(Refusal::NoSuchStream { stream: id })?;
        let state = self.streams.as_slice()[i].state;
        if !stream::sends(state) || self.streams.as_slice()[i].head_sent {
            return Err(Refusal::NotSendable { stream: id, state }.into());
        }
        head::check_outgoing_response(head).map_err(Refusal::Head)?;
        let digits = head::status_digits(head.status);
        let status = [Field::new(b":status", &digits)];
        let fields = || status.iter().copied().chain(head.fields.iter().copied());
        self.check_peer_list(fields())?;
        self.send_block(id, fields(), end)?;
        let s = &mut self.streams.as_mut_slice()[i];
        s.head_sent = true;
        if end {
            advance(s, Step::SendEnd);
        }
        self.settle(id);
        Ok(())
    }

    /// Sends trailers on `stream`, ending it.
    pub fn send_trailers(&mut self, id: StreamId, fields: &[Field<'_>]) -> Result<(), H2Error> {
        self.open_for_calls()?;
        let i = self.find(id).ok_or(Refusal::NoSuchStream { stream: id })?;
        let s = &self.streams.as_slice()[i];
        if !stream::sends(s.state) {
            return Err(Refusal::NotSendable { stream: id, state: s.state }.into());
        }
        if !s.head_sent {
            return Err(Refusal::HeadFirst { stream: id }.into());
        }
        head::check_outgoing_fields(fields).map_err(Refusal::Head)?;
        self.check_peer_list(fields.iter().copied())?;
        self.send_block(id, fields.iter().copied(), true)?;
        advance(&mut self.streams.as_mut_slice()[i], Step::SendEnd);
        self.settle(id);
        Ok(())
    }

    /// Sends what flow control and the out buffer allow of `bytes` on `stream`, and says how
    /// many bytes were taken; `end` ends the stream once every byte given is taken. A stream
    /// that took less for want of window is told [`Event::Writable`] when it has window again.
    pub fn send_body(&mut self, id: StreamId, bytes: &[u8], end: bool) -> Result<usize, H2Error> {
        self.open_for_calls()?;
        let i = self.find(id).ok_or(Refusal::NoSuchStream { stream: id })?;
        let (state, head_sent, send_window) = {
            let s = &self.streams.as_slice()[i];
            (s.state, s.head_sent, s.send_window)
        };
        if !stream::sends(state) {
            return Err(Refusal::NotSendable { stream: id, state }.into());
        }
        if !head_sent {
            return Err(Refusal::HeadFirst { stream: id }.into());
        }
        let window = send_window.min(self.conn_send).max(0);
        let room = usize::try_from(self.limits.out_buffer).unwrap_or(usize::MAX).saturating_sub(self.pending_out());
        let take = bytes.len().min(usize::try_from(window).unwrap_or(0)).min(room);
        let ends = end && take == bytes.len();
        let max_frame = usize::try_from(self.peer.max_frame_size).unwrap_or(usize::MAX).max(1);
        self.out.try_reserve(take + take.div_ceil(max_frame).max(1) * HEADER_LEN)?;
        let mut rest = &bytes[..take];
        while !rest.is_empty() {
            let (piece, after) = rest.split_at(rest.len().min(max_frame));
            frame::write_data(&mut self.out, id, piece, ends && after.is_empty())?;
            rest = after;
        }
        if ends && take == 0 {
            frame::write_data(&mut self.out, id, &[], true)?;
        }
        let taken = len_i64(take);
        self.conn_send -= taken;
        let now = self.now;
        let s = &mut self.streams.as_mut_slice()[i];
        s.send_window -= taken;
        if usize::try_from(window).unwrap_or(0) < bytes.len() {
            s.blocked_since = s.blocked_since.or(Some(now));
        } else {
            s.blocked_since = None;
        }
        if ends {
            advance(s, Step::SendEnd);
            s.blocked_since = None;
        }
        self.settle(id);
        Ok(take)
    }

    /// Moves up to `into.len()` buffered body bytes of `stream` into `into`, gives window back
    /// by the policy, and says how many.
    pub fn read(&mut self, id: StreamId, into: &mut [u8]) -> Result<usize, H2Error> {
        if self.closed {
            return Err(H2Error::Closed);
        }
        let i = self.find(id).ok_or(Refusal::NoSuchStream { stream: id })?;
        let s = &mut self.streams.as_mut_slice()[i];
        let n = s.buffered().min(into.len());
        into[..n].copy_from_slice(&s.buf.as_slice()[s.buf_at..s.buf_at + n]);
        s.buf_at += n;
        if s.buf_at == s.buf.len() {
            s.buf.clear();
            s.buf_at = 0;
        } else if s.buf_at * 2 >= s.buf.len() {
            s.buf.drain(..s.buf_at);
            s.buf_at = 0;
        }
        s.data_told = false;
        s.recv_unacked = s.recv_unacked.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        self.conn_unacked = self.conn_unacked.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        self.grant(Some(i))?;
        self.settle(id);
        Ok(n)
    }

    /// Resets `stream` with `code` (the caller no longer wants it).
    pub fn reset(&mut self, id: StreamId, code: ErrorCode) -> Result<(), H2Error> {
        self.open_for_calls()?;
        let i = self.find(id).ok_or(Refusal::NoSuchStream { stream: id })?;
        let state = self.streams.as_slice()[i].state;
        if state == State::Closed {
            return Err(Refusal::NotSendable { stream: id, state }.into());
        }
        frame::write_rst(&mut self.out, id, code)?;
        self.close_stream(i)?;
        Ok(())
    }

    /// Sends a PING with `data`; its answer is an [`Event::Pong`].
    pub fn ping(&mut self, data: [u8; 8]) -> Result<(), H2Error> {
        self.open_for_calls()?;
        if self.pings_out >= self.limits.pings {
            return Err(Refusal::PingsFull { most: self.limits.pings }.into());
        }
        frame::write_ping(&mut self.out, data, false)?;
        self.pings_out += 1;
        Ok(())
    }

    /// Sends GOAWAY with `code`: no new stream either way; streams up to the last one the peer
    /// opened go on (section 6.8).
    pub fn go_away(&mut self, code: ErrorCode) -> Result<(), H2Error> {
        self.open_for_calls()?;
        let last = StreamId(self.highest_peer);
        frame::write_goaway(&mut self.out, last, code)?;
        self.goaway_sent = Some(last);
        Ok(())
    }

    /// Takes bytes from the peer at `now`. A connection error queues GOAWAY and is returned;
    /// the machine then takes nothing more.
    pub fn received(&mut self, bytes: &[u8], now: Instant) -> Result<(), H2Error> {
        if self.closed {
            return Err(H2Error::Closed);
        }
        self.now = now;
        let result = self.ingest(bytes);
        self.finish(result)
    }

    /// Passes every deadline at or before `now`: an unacknowledged SETTINGS ends the
    /// connection; a stream stalled past its deadline is reset with CANCEL.
    pub fn timed_out(&mut self, now: Instant) -> Result<(), H2Error> {
        if self.closed {
            return Err(H2Error::Closed);
        }
        self.now = now;
        let result = self.pass_deadlines(now);
        self.finish(result)
    }

    // ---- Inside. ----

    fn open_for_calls(&self) -> Result<(), H2Error> {
        if self.closed { Err(H2Error::Closed) } else { Ok(()) }
    }

    /// A fault ends the connection with a GOAWAY; a failed allocation ends it with none (its
    /// state may be half moved).
    fn finish(&mut self, result: Result<(), H2Error>) -> Result<(), H2Error> {
        match result {
            Ok(()) => Ok(()),
            Err(H2Error::Connection(fault)) => {
                self.closed = true;
                self.fault = Some(fault);
                frame::write_goaway(&mut self.out, StreamId(self.highest_peer), fault.code)?;
                Err(H2Error::Connection(fault))
            }
            Err(other) => {
                self.closed = true;
                Err(other)
            }
        }
    }

    fn pass_deadlines(&mut self, now: Instant) -> Result<(), H2Error> {
        if let Some(at) = self.settings_sent_at
            && now >= at.after(self.limits.settings_timeout)
        {
            return Err(conn_fault(ErrorCode::SETTINGS_TIMEOUT, None, StreamId::ZERO, Rule::SettingsTimeout));
        }
        let mut i = 0;
        while i < self.streams.len() {
            let s = &self.streams.as_slice()[i];
            if let Some(since) = s.blocked_since
                && now >= since.after(self.limits.stall_timeout)
            {
                let id = s.id;
                self.owe_reset(id, ErrorCode::CANCEL)?;
                self.push_event(Event::Reset {
                    stream: id,
                    code: ErrorCode::CANCEL,
                    by: ResetBy::Us { frame: None, rule: Rule::SendStalled },
                })?;
                self.close_stream(i)?;
                continue;
            }
            i += 1;
        }
        Ok(())
    }

    fn find(&self, id: StreamId) -> Option<usize> {
        self.streams.as_slice().binary_search_by_key(&id, |s| s.id).ok()
    }

    fn new_stream(&self, id: StreamId, ours: bool) -> Stream<'a> {
        Stream {
            id,
            state: State::Idle,
            ours,
            send_window: i64::from(self.peer.initial_window_size),
            recv_window: i64::from(self.limits.stream_window),
            recv_unacked: 0,
            buf: AVec::new_in(self.alloc),
            buf_at: 0,
            content_length: None,
            received: 0,
            head_method: false,
            final_head: false,
            head_sent: false,
            informational: 0,
            data_told: false,
            blocked_since: None,
        }
    }

    /// Puts `stream` in the table in identifier order; room was reserved by the caller.
    fn insert(&mut self, stream: Stream<'a>) {
        let at = match self.streams.as_slice().binary_search_by_key(&stream.id, |s| s.id) {
            Ok(at) | Err(at) => at,
        };
        self.streams.insert_shift(at, stream);
    }

    /// Whether `id` is idle: above every identifier of its parity used so far.
    fn is_idle(&self, id: StreamId) -> bool {
        let ours = (self.role == Role::Client) == id.by_client();
        if ours { id.0 >= self.next_id } else { id.0 > self.highest_peer }
    }

    fn push_event(&mut self, event: Event<'a>) -> Result<(), H2Error> {
        self.events.try_push_back(event).map_err(|refused| H2Error::Alloc(refused.why))
    }

    /// The head's size by the peer's SETTINGS_MAX_HEADER_LIST_SIZE accounting is within it.
    fn check_peer_list<'f>(&self, fields: impl Iterator<Item = Field<'f>>) -> Result<(), H2Error> {
        let Some(most) = self.peer.max_header_list_size else { return Ok(()) };
        let size = fields.fold(0u64, |sum, f| sum.saturating_add(entry_size(f.name, f.value)));
        if size > u64::from(most) {
            return Err(Refusal::HeadOverPeerLimit { size, most }.into());
        }
        Ok(())
    }

    /// Encodes `fields` and writes them as HEADERS and CONTINUATIONs. A failure here leaves the
    /// encoder's table moved past what the peer will see, so it ends the connection.
    fn send_block<'f>(
        &mut self,
        id: StreamId,
        fields: impl Iterator<Item = Field<'f>>,
        end: bool,
    ) -> Result<(), H2Error> {
        self.scratch.clear();
        let written = self
            .encoder
            .encode(fields, &mut self.scratch)
            .and_then(|()| frame::write_headers(&mut self.out, id, self.scratch.as_slice(), end, self.peer.max_frame_size));
        if let Err(failure) = written {
            self.closed = true;
            return Err(failure.into());
        }
        Ok(())
    }

    /// Queues a RST_STREAM owed to the peer, within the cap.
    fn owe_reset(&mut self, id: StreamId, code: ErrorCode) -> Result<(), H2Error> {
        if self.resets_pending >= self.limits.pending_resets {
            return Err(conn_fault(
                ErrorCode::ENHANCE_YOUR_CALM,
                None,
                id,
                Rule::ResetsOverCap { most: self.limits.pending_resets },
            ));
        }
        self.resets_pending += 1;
        frame::write_rst(&mut self.out, id, code)?;
        Ok(())
    }

    /// A stream error (section 5.4.2): RST_STREAM with `code`, the stream closed and its
    /// buffered body dropped, the caller told.
    fn stream_error(&mut self, id: StreamId, code: ErrorCode, frame: FrameKind, rule: Rule) -> Result<(), H2Error> {
        self.owe_reset(id, code)?;
        if let Some(i) = self.find(id) {
            self.push_event(Event::Reset { stream: id, code, by: ResetBy::Us { frame: Some(frame), rule } })?;
            self.close_stream(i)?;
        }
        Ok(())
    }

    /// Closes the stream at `i`, gives back the connection window its unread body held, and
    /// drops it from the table.
    fn close_stream(&mut self, i: usize) -> Result<(), H2Error> {
        let s = self.streams.remove_shift(i);
        let dropped = u32::try_from(s.buffered()).unwrap_or(u32::MAX);
        self.conn_unacked = self.conn_unacked.saturating_add(dropped);
        self.grant(None)
    }

    /// Drops a stream that is closed and whose body is all read.
    fn settle(&mut self, id: StreamId) {
        if let Some(i) = self.find(id) {
            let s = &self.streams.as_slice()[i];
            if s.state == State::Closed && s.buffered() == 0 {
                drop(self.streams.remove_shift(i));
            }
        }
    }

    /// The window policy: a WINDOW_UPDATE once half a window has been read since the last.
    fn grant(&mut self, i: Option<usize>) -> Result<(), H2Error> {
        if self.conn_unacked >= self.limits.conn_window / 2 && self.conn_unacked > 0 {
            frame::write_window_update(&mut self.out, StreamId::ZERO, self.conn_unacked)?;
            self.conn_recv += i64::from(self.conn_unacked);
            self.conn_unacked = 0;
        }
        if let Some(i) = i {
            let half = self.limits.stream_window / 2;
            let s = &mut self.streams.as_mut_slice()[i];
            if stream::receives(s.state) && s.recv_unacked >= half && s.recv_unacked > 0 {
                let (id, inc) = (s.id, s.recv_unacked);
                s.recv_window += i64::from(inc);
                s.recv_unacked = 0;
                frame::write_window_update(&mut self.out, id, inc)?;
            }
        }
        Ok(())
    }

    /// Tells a blocked stream it may send again, when both windows have room.
    fn wake(&mut self, i: usize) -> Result<(), H2Error> {
        let conn_open = self.conn_send > 0;
        let s = &mut self.streams.as_mut_slice()[i];
        if s.blocked_since.is_some() && s.send_window > 0 && conn_open && stream::sends(s.state) {
            s.blocked_since = None;
            let id = s.id;
            self.push_event(Event::Writable { stream: id })?;
        }
        Ok(())
    }

    fn wake_all(&mut self) -> Result<(), H2Error> {
        for i in 0..self.streams.len() {
            self.wake(i)?;
        }
        Ok(())
    }

    fn check_len(&self, header: Header) -> Result<(), H2Error> {
        if header.len > self.limits.frame_size {
            return Err(conn_fault(
                ErrorCode::FRAME_SIZE_ERROR,
                Some(header.kind),
                header.stream,
                Rule::FrameOverMax { len: header.len, most: self.limits.frame_size },
            ));
        }
        Ok(())
    }

    fn ingest(&mut self, mut input: &[u8]) -> Result<(), H2Error> {
        while self.preface_left > 0 {
            if input.is_empty() {
                return Ok(());
            }
            let at = PREFACE.len() - self.preface_left;
            let take = self.preface_left.min(input.len());
            if input[..take] != PREFACE[at..at + take] {
                return Err(conn_fault(ErrorCode::PROTOCOL_ERROR, None, StreamId::ZERO, Rule::Preface));
            }
            self.preface_left -= take;
            input = &input[take..];
        }
        if !self.inbuf.is_empty() {
            if self.inbuf.len() < HEADER_LEN {
                let take = (HEADER_LEN - self.inbuf.len()).min(input.len());
                self.inbuf.try_extend_from_slice(&input[..take])?;
                input = &input[take..];
                if self.inbuf.len() < HEADER_LEN {
                    return Ok(());
                }
            }
            let Some(head) = self.inbuf.as_slice().first_chunk::<HEADER_LEN>() else {
                return Ok(());
            };
            let header = frame::parse_header(head);
            self.check_len(header)?;
            let total = HEADER_LEN + usize::try_from(header.len).unwrap_or(usize::MAX);
            let take = (total - self.inbuf.len()).min(input.len());
            self.inbuf.try_extend_from_slice(&input[..take])?;
            input = &input[take..];
            if self.inbuf.len() < total {
                return Ok(());
            }
            let buf = core::mem::replace(&mut self.inbuf, AVec::new_in(self.alloc));
            let result = self.frame(header, &buf.as_slice()[HEADER_LEN..total]);
            self.inbuf = buf;
            self.inbuf.clear();
            result?;
        }
        while let Some(head) = input.first_chunk::<HEADER_LEN>() {
            let header = frame::parse_header(head);
            self.check_len(header)?;
            let total = HEADER_LEN + usize::try_from(header.len).unwrap_or(usize::MAX);
            if input.len() < total {
                break;
            }
            self.frame(header, &input[HEADER_LEN..total])?;
            input = &input[total..];
        }
        if !input.is_empty() {
            self.inbuf.try_extend_from_slice(input)?;
        }
        Ok(())
    }

    fn frame(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if !self.got_settings && (h.kind != FrameKind::Settings || h.has(flags::ACK)) {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::FirstNotSettings));
        }
        if let Some(open) = self.block_stream
            && (h.kind != FrameKind::Continuation || h.stream != open)
        {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::ContinuationExpected));
        }
        match h.kind {
            FrameKind::Data => self.on_data(h, payload),
            FrameKind::Headers => self.on_headers(h, payload),
            FrameKind::Priority => self.on_priority(h, payload),
            FrameKind::RstStream => self.on_rst(h, payload),
            FrameKind::Settings => self.on_settings(h, payload),
            FrameKind::PushPromise => {
                if h.stream.is_zero() {
                    return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamZero));
                }
                Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::PushRefused))
            }
            FrameKind::Ping => self.on_ping(h, payload),
            FrameKind::GoAway => self.on_goaway(h, payload),
            FrameKind::WindowUpdate => self.on_window_update(h, payload),
            FrameKind::Continuation => self.on_continuation(h, payload),
            // Section 5.5: "Implementations MUST ignore and discard frames of unknown types."
            FrameKind::Unknown(_) => Ok(()),
        }
    }

    fn on_data(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamZero));
        }
        let body = frame::unpad(h, payload).map_err(|rule| fault(ErrorCode::PROTOCOL_ERROR, rule))?;
        // Section 6.9.1: "The entire DATA frame payload is included in flow control, including
        // the Pad Length and Padding fields if present."
        let len = h.len;
        if i64::from(len) > self.conn_recv {
            return Err(fault(ErrorCode::FLOW_CONTROL_ERROR, Rule::FlowControl { len, window: self.conn_recv }));
        }
        self.conn_recv -= i64::from(len);
        let Some(i) = self.find(h.stream) else {
            if self.is_idle(h.stream) {
                return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::Idle));
            }
            // Closed: discarded, its bytes given back to the connection at once.
            self.conn_unacked = self.conn_unacked.saturating_add(len);
            return self.grant(None);
        };
        let state = self.streams.as_slice()[i].state;
        if !stream::receives(state) {
            self.conn_unacked = self.conn_unacked.saturating_add(len);
            self.grant(None)?;
            if state == State::HalfClosedRemote {
                return self.stream_error(h.stream, ErrorCode::STREAM_CLOSED, h.kind, Rule::HalfClosedRemote);
            }
            return Ok(());
        }
        let window = self.streams.as_slice()[i].recv_window;
        if i64::from(len) > window {
            self.conn_unacked = self.conn_unacked.saturating_add(len);
            self.grant(None)?;
            return self.stream_error(h.stream, ErrorCode::FLOW_CONTROL_ERROR, h.kind, Rule::FlowControl { len, window });
        }
        let padding = len - u32::try_from(body.len()).unwrap_or(len);
        let ends = h.has(flags::END_STREAM);
        let s = &mut self.streams.as_mut_slice()[i];
        s.recv_window -= i64::from(len);
        s.received += u64::try_from(body.len()).unwrap_or(u64::MAX);
        let length_wrong = match s.content_length {
            Some(said) => s.received > said || (ends && s.received != said),
            None => false,
        };
        if length_wrong {
            let (said, got) = (s.content_length.unwrap_or(0), s.received);
            self.conn_unacked = self.conn_unacked.saturating_add(len);
            self.grant(None)?;
            return self.stream_error(h.stream, ErrorCode::PROTOCOL_ERROR, h.kind, Rule::ContentLength { said, got });
        }
        s.buf.try_extend_from_slice(body)?;
        s.recv_unacked = s.recv_unacked.saturating_add(padding);
        let tell_data = !body.is_empty() && !s.data_told;
        if tell_data {
            s.data_told = true;
        }
        if ends {
            advance(s, Step::RecvEnd);
        }
        self.conn_unacked = self.conn_unacked.saturating_add(padding);
        if tell_data {
            self.push_event(Event::Data { stream: h.stream })?;
        }
        if ends {
            self.push_event(Event::End { stream: h.stream })?;
        }
        self.grant(Some(i))?;
        self.settle(h.stream);
        Ok(())
    }

    fn on_headers(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamZero));
        }
        let (priority, fragment) = frame::headers_payload(h, payload).map_err(|rule| match rule {
            Rule::Length { .. } => fault(ErrorCode::FRAME_SIZE_ERROR, rule),
            _ => fault(ErrorCode::PROTOCOL_ERROR, rule),
        })?;
        self.block_error = match priority {
            Some(p) if p.depends_on == h.stream => Some((ErrorCode::PROTOCOL_ERROR, Rule::SelfDependency)),
            _ => None,
        };
        self.block.clear();
        self.block_stream = Some(h.stream);
        self.block_end_stream = h.has(flags::END_STREAM);
        self.block_frames = 0;
        self.gather(h, fragment)
    }

    fn on_continuation(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if self.block_stream.is_none() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::ContinuationUnexpected));
        }
        self.block_frames = self.block_frames.saturating_add(1);
        if self.block_frames > self.limits.continuations {
            return Err(fault(ErrorCode::ENHANCE_YOUR_CALM, Rule::ContinuationsOverCap { most: self.limits.continuations }));
        }
        self.gather(h, payload)
    }

    /// Adds a fragment to the block, within the cap; decodes it at END_HEADERS.
    fn gather(&mut self, h: Header, fragment: &[u8]) -> Result<(), H2Error> {
        let most = self.limits.header_block;
        if self.block.len() + fragment.len() > usize::try_from(most).unwrap_or(usize::MAX) {
            return Err(conn_fault(
                ErrorCode::ENHANCE_YOUR_CALM,
                Some(h.kind),
                h.stream,
                Rule::HeaderBlockOverCap { most },
            ));
        }
        self.block.try_extend_from_slice(fragment)?;
        if !h.has(flags::END_HEADERS) {
            return Ok(());
        }
        let Some(id) = self.block_stream.take() else { return Ok(()) };
        let end = self.block_end_stream;
        let mut fields = Fields::new_in(self.alloc);
        let block = core::mem::replace(&mut self.block, AVec::new_in(self.alloc));
        let decoded = self.decoder.decode(block.as_slice(), &mut fields, self.limits.header_list);
        self.block = block;
        self.block.clear();
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(Halt::Fault(why)) => {
                return Err(conn_fault(ErrorCode::COMPRESSION_ERROR, Some(h.kind), id, Rule::Compression(why)));
            }
            Err(Halt::Alloc(failure)) => return Err(failure.into()),
        };
        if let Some((code, rule)) = self.block_error.take() {
            return self.stream_error(id, code, FrameKind::Headers, rule);
        }
        self.on_head(id, fields, end, decoded.over)
    }

    /// A whole head received on `id`.
    fn on_head(&mut self, id: StreamId, fields: Fields<'a>, end: bool, over: bool) -> Result<(), H2Error> {
        let kind = FrameKind::Headers;
        let fault = |code, rule| conn_fault(code, Some(kind), id, rule);
        let Some(i) = self.find(id) else {
            if !self.is_idle(id) {
                // Closed: HPACK has taken the block; the head is discarded (section 5.1).
                return Ok(());
            }
            if self.role == Role::Client {
                // A server opens no stream without PUSH_PROMISE, which we refused.
                return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::Idle));
            }
            return self.on_new_request(id, fields, end, over);
        };
        let state = self.streams.as_slice()[i].state;
        if !stream::receives(state) {
            if state == State::HalfClosedRemote {
                return self.stream_error(id, ErrorCode::STREAM_CLOSED, kind, Rule::HalfClosedRemote);
            }
            return Ok(());
        }
        if over {
            let most = self.limits.header_list;
            return self.stream_error(id, ErrorCode::ENHANCE_YOUR_CALM, kind, Rule::HeaderListOverCap { most });
        }
        let final_head = self.streams.as_slice()[i].final_head;
        if self.role == Role::Client && !final_head {
            let checked = match head::check_response(&fields) {
                Ok(checked) => checked,
                Err(why) => return self.stream_error(id, ErrorCode::PROTOCOL_ERROR, kind, Rule::Malformed(why)),
            };
            if checked.status < 200 {
                if end {
                    let rule = Rule::Malformed(head::HeadWhy::InformationalEnd);
                    return self.stream_error(id, ErrorCode::PROTOCOL_ERROR, kind, rule);
                }
                let most = self.limits.informational;
                let s = &mut self.streams.as_mut_slice()[i];
                s.informational = s.informational.saturating_add(1);
                if s.informational > most {
                    return self.stream_error(id, ErrorCode::ENHANCE_YOUR_CALM, kind, Rule::InformationalOverCap { most });
                }
                return self.push_event(Event::Headers { stream: id, fields, end: false });
            }
            let s = &mut self.streams.as_mut_slice()[i];
            s.final_head = true;
            s.content_length = if s.head_method || checked.status == 204 || checked.status == 304 {
                Some(0)
            } else {
                checked.content_length
            };
        } else {
            // Trailers: a head after the final one (both roles).
            if !end {
                let rule = Rule::Malformed(head::HeadWhy::TrailersNotEnd);
                return self.stream_error(id, ErrorCode::PROTOCOL_ERROR, kind, rule);
            }
            if let Err(why) = head::check_trailers(&fields) {
                return self.stream_error(id, ErrorCode::PROTOCOL_ERROR, kind, Rule::Malformed(why));
            }
        }
        self.head_arrived(i, fields, end)
    }

    /// A head that passed its checks: the body's length checked when it ends the stream, the
    /// caller told.
    fn head_arrived(&mut self, i: usize, fields: Fields<'a>, end: bool) -> Result<(), H2Error> {
        let s = &mut self.streams.as_mut_slice()[i];
        let id = s.id;
        if end && let Some(said) = s.content_length
            && s.received != said
        {
            let got = s.received;
            return self.stream_error(id, ErrorCode::PROTOCOL_ERROR, FrameKind::Headers, Rule::ContentLength { said, got });
        }
        if end {
            advance(s, Step::RecvEnd);
        }
        self.push_event(Event::Headers { stream: id, fields, end })?;
        if end {
            self.push_event(Event::End { stream: id })?;
        }
        self.settle(id);
        Ok(())
    }

    /// A request on a new stream (server role).
    fn on_new_request(&mut self, id: StreamId, fields: Fields<'a>, end: bool, over: bool) -> Result<(), H2Error> {
        let kind = FrameKind::Headers;
        let fault = |code, rule| conn_fault(code, Some(kind), id, rule);
        if !id.by_client() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamIdParity { id: id.0 }));
        }
        self.highest_peer = id.0;
        if self.goaway_sent.is_some() {
            // Section 6.8: streams above the last one we named are not processed.
            return Ok(());
        }
        if over {
            let most = self.limits.header_list;
            return self.refuse_new(id, ErrorCode::ENHANCE_YOUR_CALM, Rule::HeaderListOverCap { most });
        }
        let open = self.streams.iter().filter(|s| !s.ours).count();
        if open >= usize::try_from(self.limits.concurrent_streams).unwrap_or(usize::MAX) {
            let most = self.limits.concurrent_streams;
            return self.refuse_new(id, ErrorCode::REFUSED_STREAM, Rule::StreamsOverCap { most });
        }
        let checked = match head::check_request(&fields) {
            Ok(checked) => checked,
            Err(why) => return self.refuse_new(id, ErrorCode::PROTOCOL_ERROR, Rule::Malformed(why)),
        };
        if end && checked.content_length.is_some_and(|n| n != 0) {
            let said = checked.content_length.unwrap_or(0);
            return self.refuse_new(id, ErrorCode::PROTOCOL_ERROR, Rule::ContentLength { said, got: 0 });
        }
        let mut stream = self.new_stream(id, false);
        stream.state = State::Open;
        stream.final_head = true;
        stream.content_length = checked.content_length;
        stream.head_method = checked.head_method;
        if end {
            advance(&mut stream, Step::RecvEnd);
        }
        self.streams.try_reserve(1)?;
        self.insert(stream);
        self.push_event(Event::Headers { stream: id, fields, end })?;
        if end {
            self.push_event(Event::End { stream: id })?;
        }
        Ok(())
    }

    /// A new stream refused before the caller saw it: RST_STREAM, no event.
    fn refuse_new(&mut self, id: StreamId, code: ErrorCode, _rule: Rule) -> Result<(), H2Error> {
        self.owe_reset(id, code)
    }

    fn on_priority(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamZero));
        }
        let known = self.find(h.stream).is_some();
        let Some(five) = payload.first_chunk::<5>().filter(|_| payload.len() == 5) else {
            // Section 6.3: a stream error FRAME_SIZE_ERROR; on a stream we hold no record of
            // (idle or closed) a RST_STREAM may not be sent (section 5.1), so the connection's.
            if known {
                return self.stream_error(h.stream, ErrorCode::FRAME_SIZE_ERROR, h.kind, Rule::Length { len: h.len });
            }
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        };
        if frame::parse_priority(*five).depends_on == h.stream {
            if known {
                return self.stream_error(h.stream, ErrorCode::PROTOCOL_ERROR, h.kind, Rule::SelfDependency);
            }
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::SelfDependency));
        }
        // Parsed and ignored (section 5.3.2).
        Ok(())
    }

    fn on_rst(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamZero));
        }
        if payload.len() != 4 {
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        }
        if self.is_idle(h.stream) {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::Idle));
        }
        if !self.charge_reset() {
            return Err(fault(ErrorCode::ENHANCE_YOUR_CALM, Rule::RapidReset { most: self.limits.peer_resets }));
        }
        let code = ErrorCode(frame::be32(payload, 0));
        if let Some(i) = self.find(h.stream) {
            self.push_event(Event::Reset { stream: h.stream, code, by: ResetBy::Peer })?;
            self.close_stream(i)?;
        }
        Ok(())
    }

    /// Takes one reset from the rapid-reset budget: `peer_resets` per window, refilled evenly.
    fn charge_reset(&mut self) -> bool {
        let window = window_nanos(&self.limits);
        let most = u64::from(self.limits.peer_resets);
        let elapsed = u64::try_from(self.now.since(self.reset_refilled).as_nanos()).unwrap_or(u64::MAX).min(window);
        self.reset_tokens = self.reset_tokens.saturating_add(elapsed.saturating_mul(most)).min(most * window);
        self.reset_refilled = self.now;
        if self.reset_tokens < window {
            return false;
        }
        self.reset_tokens -= window;
        true
    }

    fn on_settings(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if !h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamNotZero));
        }
        if h.has(flags::ACK) {
            if !payload.is_empty() {
                return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
            }
            if self.settings_sent_at.take().is_none() {
                return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::SettingsAckUnasked));
            }
            self.decoder.set_allowed(self.limits.hpack_table);
            return self.push_event(Event::SettingsAcked);
        }
        if payload.len() % 6 != 0 {
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        }
        if self.acks_pending >= self.limits.pending_acks {
            return Err(fault(ErrorCode::ENHANCE_YOUR_CALM, Rule::AcksOverCap { most: self.limits.pending_acks }));
        }
        let mut new = self.peer;
        for entry in frame::settings_entries(payload) {
            new.apply(entry, self.role).map_err(|(rule, code)| fault(code, rule))?;
        }
        let delta = i64::from(new.initial_window_size) - i64::from(self.peer.initial_window_size);
        if delta != 0 {
            for s in self.streams.iter_mut() {
                let moved = s.send_window + delta;
                if moved > i64::from(MAX_31) {
                    return Err(fault(ErrorCode::FLOW_CONTROL_ERROR, Rule::WindowOverflow));
                }
                s.send_window = moved;
            }
        }
        self.encoder.peer_allows(new.header_table_size);
        self.peer = new;
        self.got_settings = true;
        self.acks_pending += 1;
        frame::write_settings_ack(&mut self.out)?;
        if self.settings_told {
            for event in self.events.iter_mut() {
                if let Event::Settings(told) = event {
                    *told = new;
                }
            }
        } else {
            self.settings_told = true;
            self.push_event(Event::Settings(new))?;
        }
        if delta > 0 {
            self.wake_all()?;
        }
        Ok(())
    }

    fn on_ping(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if !h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamNotZero));
        }
        let Some(&data) = payload.first_chunk::<8>().filter(|_| payload.len() == 8) else {
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        };
        if h.has(flags::ACK) {
            // Section 6.7: "Receivers of a PING frame that has the ACK flag set MUST NOT respond";
            // one we did not ask for is told to nobody.
            if self.pings_out > 0 {
                self.pings_out -= 1;
                return self.push_event(Event::Pong { data });
            }
            return Ok(());
        }
        if self.acks_pending >= self.limits.pending_acks {
            return Err(fault(ErrorCode::ENHANCE_YOUR_CALM, Rule::AcksOverCap { most: self.limits.pending_acks }));
        }
        self.acks_pending += 1;
        frame::write_ping(&mut self.out, data, true)?;
        Ok(())
    }

    fn on_goaway(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if !h.stream.is_zero() {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::StreamNotZero));
        }
        if payload.len() < 8 {
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        }
        let last = StreamId(frame::be32(payload, 0) & MAX_31);
        let code = ErrorCode(frame::be32(payload, 4));
        if let Some(before) = self.goaway_received
            && last > before
        {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::GoAwayLastIncreased { before: before.0, now: last.0 }));
        }
        self.goaway_received = Some(last);
        let mut i = 0;
        while i < self.streams.len() {
            let s = &self.streams.as_slice()[i];
            if s.ours && s.id > last {
                let id = s.id;
                self.push_event(Event::Reset { stream: id, code: ErrorCode::REFUSED_STREAM, by: ResetBy::GoAway })?;
                self.close_stream(i)?;
                continue;
            }
            i += 1;
        }
        self.push_event(Event::GoAway { last, code })
    }

    fn on_window_update(&mut self, h: Header, payload: &[u8]) -> Result<(), H2Error> {
        let fault = |code, rule| conn_fault(code, Some(h.kind), h.stream, rule);
        if payload.len() != 4 {
            return Err(fault(ErrorCode::FRAME_SIZE_ERROR, Rule::Length { len: h.len }));
        }
        let increment = frame::be32(payload, 0) & MAX_31;
        if h.stream.is_zero() {
            if increment == 0 {
                return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::WindowIncrementZero));
            }
            let moved = self.conn_send + i64::from(increment);
            if moved > i64::from(MAX_31) {
                return Err(fault(ErrorCode::FLOW_CONTROL_ERROR, Rule::WindowOverflow));
            }
            self.conn_send = moved;
            return self.wake_all();
        }
        if self.is_idle(h.stream) {
            return Err(fault(ErrorCode::PROTOCOL_ERROR, Rule::Idle));
        }
        let Some(named) = self.find(h.stream) else {
            // Closed: may come after our END_STREAM or RST_STREAM (section 5.1).
            return Ok(());
        };
        if increment == 0 {
            return self.stream_error(h.stream, ErrorCode::PROTOCOL_ERROR, h.kind, Rule::WindowIncrementZero);
        }
        let i = if plant::on(Plant::WindowUpdateWrongStream) && named > 0 { named - 1 } else { named };
        let moved = self.streams.as_slice()[i].send_window + i64::from(increment);
        if moved > i64::from(MAX_31) {
            let id = self.streams.as_slice()[i].id;
            return self.stream_error(id, ErrorCode::FLOW_CONTROL_ERROR, h.kind, Rule::WindowOverflow);
        }
        self.streams.as_mut_slice()[i].send_window = moved;
        self.wake(i)
    }
}

/// The rapid-reset window in nanoseconds, at least 1.
fn window_nanos(limits: &Limits) -> u64 {
    u64::try_from(limits.peer_resets_window.as_nanos()).unwrap_or(u64::MAX / 2).clamp(1, u64::MAX / (1 << 17))
}

/// Moves `s` along `step`. Every caller checked [`stream::sends`] or [`stream::receives`] first,
/// so the edge exists in the table; a missing one leaves the state as it was.
fn advance(s: &mut Stream<'_>, step: Step) {
    if let Some(next) = stream::next(s.state, step) {
        s.state = next;
    }
}
