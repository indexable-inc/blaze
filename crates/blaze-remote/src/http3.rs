//! Anonymous reads of the public cache over HTTP/3: a [`Bucket`] that runs blaze-http's sans-IO
//! HTTP/3 core ([`Endpoint`]) itself, on a [`Wire`] (datagrams and a clock) a driver gives it.
//! It is chosen by `bucket.transport = "h3"` ([`crate::settings::Transport`]).
//!
//! - Only an anonymous read goes here: a GET or HEAD of a `public/` object, to the bucket's
//!   public address ([`crate::http::anonymous`]). Everything signed stays on HTTP/1.1
//!   ([`crate::http`]); [`Routed`] splits one machine's asks between the two.
//! - One QUIC connection to the origin, opened on the first ask and opened again only once it
//!   has closed (each ask on the closed one was already answered, so this is not a retry).
//! - Every read is asked as early data ([`Early`]): with a ticket kept from an earlier process
//!   its HEADERS leave in the first flight (0 round trips to the request), without one they
//!   leave as the handshake completes. Reads are of immutable, content-addressed objects
//!   checked against signed statements, so a replayed GET is harmless; that is why early data
//!   is allowed here and nowhere else.
//! - Each ask carries its machine's deadline into the core, which answers it
//!   [`RequestEnd::TimedOut`] past it: [`Reply::TimedOut`] here.
//! - No fallback: a QUIC connection that cannot be made fails each read on it,
//!   [`Reply::Failed`] naming the object, the host, how the handshake failed (timed out,
//!   version negotiation, ALPN refused) and [`TRANSPORT_SETTING`] to change. Nothing is asked
//!   again, over HTTP/3 or HTTP/1.1.
//!
//! Tickets: the bucket starts with the tickets a driver read from [`TICKETS_FILE`] (see
//! [`decode_tickets`]) and hands out the ones its servers send ([`H3Bucket::take_tickets`]),
//! for the driver to write back ([`encode_tickets`]) owner-only, as credentials are: a ticket's
//! bytes resume a session as this client. The file holds at most [`TICKETS_KEPT`] tickets, none
//! older than [`TICKET_AGE_MOST_S`].
//!
//! This module does no IO and reads no clock: the [`Wire`] does both.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, collections::BTreeMap, net::SocketAddr, time::Duration};

use blaze_http::h3::{
    self, ClientConfig, ConnId, Early, Endpoint, Event, Length, Range, RequestEnd, RequestId,
    ResponseHead, Trust, Wire,
};

use crate::{
    bucket::{Asked, Bucket, Counts, Op, Reply, Ticket},
    http::{Want, anonymous, reply},
    s3,
    settings::TRANSPORT_SETTING,
};

/// The file, in the settings directory ([`crate::settings::Settings::dir`]), that keeps the
/// HTTP/3 tickets between processes. Owner-only (mode 0600), as the credentials file is.
pub const TICKETS_FILE: &str = "remote-tickets.bin";
/// The most tickets kept: what the core keeps for one server
/// ([`h3::MOST_TICKETS_PER_SERVER`]), since the bucket talks to one origin.
pub const TICKETS_KEPT: usize = h3::MOST_TICKETS_PER_SERVER;
/// The oldest a kept ticket may be, in seconds: TLS 1.3's cap on a ticket's lifetime
/// (RFC 8446, 4.6.1: 604,800 s, seven days). A ticket's own lifetime, if shorter, wins.
pub const TICKET_AGE_MOST_S: u64 = 7 * 86_400;
/// The most bytes of one kept ticket: a session's secret, the server's ticket and its
/// certificate chain fit in far less; past it, a file is not one this blaze wrote.
pub const TICKET_BYTES_MOST: usize = 64 << 10;
/// The longest server name kept: a DNS name is at most 253 bytes.
pub const TICKET_NAME_MOST: usize = 253;
/// The tickets file's first bytes, then its layout's version ([`TICKETS_LAYOUT`]).
pub const TICKETS_MAGIC: &[u8; 16] = b"blaze h3 tickets";
/// The tickets file's layout: after [`TICKETS_MAGIC`] and this byte, a count byte, then each
/// ticket as its name's length (u16 LE) and name, its issue time (u64 LE, Unix seconds), its
/// lifetime (u32 LE, seconds), and its bytes' length (u32 LE) and bytes.
pub const TICKETS_LAYOUT: u8 = 1;
/// The QUIC idle timeout of the bucket's connection: shorter than an ask's base allowance
/// ([`crate::bucket::BASE_MS`]), so a host that never answers over UDP fails as a handshake
/// time-out that names the setting, before the ask's own deadline would answer it only
/// [`Reply::TimedOut`].
pub const QUIC_IDLE: Duration = Duration::from_secs(10);
/// How often a quiet connection is pinged: a third of [`QUIC_IDLE`], so two lost pings do not
/// close a live connection between a machine's waves.
pub const QUIC_KEEP_ALIVE: Duration = Duration::from_secs(3);
/// The most turns one [`Bucket::wait`] takes. Each turn hands the core datagrams or reaches a
/// deadline the core holds, and every ask's deadline is among them, so a wait ends long before
/// this; past it the core is not making progress, and every read waiting fails naming it.
pub const TURNS_MOST_PER_WAIT: u64 = 1 << 20;
/// How long one wait on HTTP/3 lasts before [`Routed`] looks for HTTP/1.1 answers, when both
/// have asks in flight: the HTTP/1.1 driver answers on its own runtime, which a wait on the
/// wire is not woken by. Well under a round trip to any bucket, so neither side's answers
/// wait on the other. It goes when both drivers run on blaze-core's one event loop and one
/// wait covers both.
pub const MIXED_SLICE: Duration = Duration::from_millis(2);

/// The port of an `https://` address that names none (RFC 9110, 4.2.2), and the UDP port
/// HTTP/3 is reached on there.
pub const HTTPS_PORT: u16 = 443;

/// The public address's origin: its host (what TLS checks and tickets are kept by), its
/// authority (`host[:port]`, the `:authority` sent) and its port. The address is
/// `https://<host>[:<port>]` with no path, as [`crate::settings`] checks.
pub fn origin(public_url: &str) -> Result<(String, String, u16), String> {
    let refused = || format!("the public address {public_url:?} is not https://<host>[:<port>]");
    let authority = public_url.strip_prefix("https://").ok_or_else(refused)?.trim_end_matches('/');
    if authority.is_empty() || authority.contains(['/', '@', '?', '#']) {
        return Err(refused());
    }
    // An IPv6 literal is bracketed, as in a URL; otherwise a port follows the last colon.
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let (host, rest) = bracketed.split_once(']').ok_or_else(refused)?;
            match rest {
                "" => (host, HTTPS_PORT),
                _ => {
                    let port = rest.strip_prefix(':').ok_or_else(refused)?;
                    (host, port.parse::<u16>().map_err(|_| refused())?)
                }
            }
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().map_err(|_| refused())?),
            None => (authority, HTTPS_PORT),
        },
    };
    if host.is_empty() {
        return Err(refused());
    }
    Ok((host.to_owned(), authority.to_owned(), port))
}

/// A read asked of the core, until it ends.
struct Pending {
    ticket: Ticket,
    object: String,
    want: Want,
    head: Option<ResponseHead>,
    body: Vec<u8>,
}

/// The bucket's anonymous reads over HTTP/3, on `W`.
pub struct H3Bucket<W: Wire> {
    wire: W,
    endpoint: Endpoint,
    peer: SocketAddr,
    host: String,
    authority: String,
    public_url: String,
    conn: Option<ConnId>,
    pending: BTreeMap<RequestId, Pending>,
    answered: Vec<(Ticket, Reply)>,
    /// Tickets the server sent, newest last, at most [`TICKETS_KEPT`].
    tickets: Vec<h3::Ticket, Global>,
    counts: Counts,
}

impl<W: Wire> H3Bucket<W> {
    /// Reads of `public_url`, whose origin the driver resolved to `peer`, trusting `trust`
    /// (the machine's roots, for a real bucket), resuming with the `kept` tickets for its host.
    pub fn new(
        wire: W,
        public_url: &str,
        peer: SocketAddr,
        trust: Trust,
        kept: &[h3::Ticket],
    ) -> Result<H3Bucket<W>, String> {
        let (host, authority, _) = origin(public_url)?;
        let transport = h3::Transport { idle: QUIC_IDLE, keep_alive: Some(QUIC_KEEP_ALIVE) };
        let config = ClientConfig { trust, transport };
        let mut endpoint = Endpoint::new(wire.epoch(), None, Some(config))
            .map_err(|e| format!("the HTTP/3 client for {host}: {e}"))?;
        for ticket in kept.iter().filter(|it| it.server_name == host) {
            endpoint.add_ticket(ticket).map_err(|e| {
                format!("{TICKETS_FILE}: a ticket for {host} is not one this blaze reads ({e}): delete the file")
            })?;
        }
        Ok(H3Bucket {
            wire,
            endpoint,
            peer,
            host,
            authority,
            public_url: public_url.trim_end_matches('/').to_owned(),
            conn: None,
            pending: BTreeMap::new(),
            answered: Vec::new_in(Global),
            tickets: Vec::new_in(Global),
            counts: Counts::default(),
        })
    }

    /// The tickets the server sent since the last call, oldest first: the driver writes them
    /// to [`TICKETS_FILE`].
    pub fn take_tickets(&mut self) -> Vec<h3::Ticket, Global> {
        std::mem::replace(&mut self.tickets, Vec::new_in(Global))
    }

    /// The wire, for a driver or a test to look at.
    pub fn wire(&mut self) -> &mut W {
        &mut self.wire
    }

    /// The wire back, the bucket done with: what a test hands the next process's bucket.
    pub fn into_wire(self) -> W {
        self.wire
    }

    /// Asks `op` of the core by `deadline` (on the wire's clock); how to read its answer.
    fn ask(&mut self, op: &Op, deadline: Duration) -> Result<(RequestId, Want), String> {
        let name = op.name();
        if !anonymous(op) {
            return Err(format!(
                "{name}: only an anonymous read of a public object goes over HTTP/3; a signed request goes over HTTP/1.1"
            ));
        }
        let url = s3::public(&self.public_url, name);
        let path = url[self.public_url.len()..].to_owned();
        let authority = self.authority.clone();
        let (early, want) = match *op {
            Op::Get { range: Some((at, len)), most, .. } => {
                let range = Range { at, len: len.max(1) };
                let length = Length::AtMost(most.min(len));
                (Early::get(authority, path, Some(range), length, deadline), Want::Get { ranged: true })
            }
            Op::Get { most, .. } => {
                (Early::get(authority, path, None, Length::AtMost(most), deadline), Want::Get { ranged: false })
            }
            _ => (Early::head(authority, path, deadline), Want::Head),
        };
        let conn = match self.conn {
            Some(conn) => conn,
            None => {
                let conn = self
                    .endpoint
                    .connect_requests(self.peer, &self.host)
                    .map_err(|e| self.quic_failed(name, &e.to_string()))?;
                self.conn = Some(conn);
                conn
            }
        };
        let request = self.endpoint.request_early(conn, early).map_err(|e| format!("{name}: {e}"))?;
        Ok((request, want))
    }

    /// Why a read failed because its QUIC connection did: the object, the host, how, and the
    /// setting that chose HTTP/3.
    fn quic_failed(&self, object: &str, why: &str) -> String {
        format!(
            "{object}: HTTP/3 to {}: {why}. Anonymous reads go over HTTP/3 because {TRANSPORT_SETTING} is \"h3\"; set it to \"h1\" to read over HTTP/1.1",
            self.host
        )
    }

    /// The reply to a read that ended `end`.
    fn ended(&self, pending: Pending, end: RequestEnd) -> Reply {
        let Pending { object, want, head, body, .. } = pending;
        match end {
            RequestEnd::Done => {
                let Some(head) = head else {
                    return Reply::Failed { why: format!("{object}: the HTTP/3 answer ended with no head") };
                };
                let mut headers = Vec::new_in(Global);
                if let Some(etag) = head.etag {
                    headers.push(("etag".to_owned(), etag));
                }
                if let Some(len) = head.content_length {
                    headers.push(("content-length".to_owned(), len.to_string()));
                }
                reply(&object, want, head.status, &headers, body)
            }
            RequestEnd::TimedOut => Reply::TimedOut,
            RequestEnd::NotStarted { why } => Reply::Failed { why: self.quic_failed(&object, &why) },
            RequestEnd::Lost(error) => Reply::Failed { why: self.quic_failed(&object, &error.why) },
            RequestEnd::Failed(error) => Reply::Failed { why: format!("{object}: {error}") },
            RequestEnd::Reset { code } => Reply::Failed {
                why: format!("{object}: {} reset the request (HTTP/3 error {code:#x})", self.host),
            },
            // This bucket never cancels: a machine's ask is answered or times out.
            RequestEnd::Cancelled => Reply::Failed { why: format!("{object}: the read was cancelled") },
        }
    }

    /// Every datagram the core has, onto the wire.
    fn flush(&mut self) {
        while let Some(transmit) = self.endpoint.next_transmit() {
            self.wire.send(transmit);
        }
    }

    /// Every event of the core, taken.
    fn pump(&mut self) {
        while let Some(event) = self.endpoint.next_event() {
            match event {
                Event::ResponseHead { request, head } => {
                    if let Some(pending) = self.pending.get_mut(&request) {
                        pending.head = Some(head);
                    }
                }
                Event::ResponseData { request, bytes } => {
                    self.endpoint.consumed_response(request, bytes.len());
                    if let Some(pending) = self.pending.get_mut(&request) {
                        pending.body.extend_from_slice(&bytes);
                    }
                }
                Event::RequestEnded { request, end } => {
                    if let Some(pending) = self.pending.remove(&request) {
                        let ticket = pending.ticket;
                        let reply = self.ended(pending, end);
                        if let Reply::Got { bytes, .. } = &reply {
                            self.counts.received += bytes.len() as u64;
                        }
                        self.answered.push((ticket, reply));
                    }
                }
                Event::ConnectionClosed { conn, .. } => {
                    if self.conn == Some(conn) {
                        self.conn = None;
                    }
                }
                Event::Ticket { ticket } => {
                    if self.tickets.len() >= TICKETS_KEPT {
                        self.tickets.remove(0);
                    }
                    self.tickets.push(ticket);
                }
                // A client endpoint serves nothing, and the rest are WebTransport's.
                _ => {}
            }
        }
    }

    /// Fails every read still waiting, saying `why`.
    fn fail_pending(&mut self, why: &str) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            let reply = Reply::Failed { why: format!("{}: {why}", pending.object) };
            self.answered.push((pending.ticket, reply));
        }
    }

    /// Waits until a read is answered, or the wire's clock reaches `cap`; every answer ready
    /// (none when `cap` came first).
    pub fn wait_until(&mut self, cap: Option<Duration>) -> Vec<(Ticket, Reply)> {
        for _ in 0..TURNS_MOST_PER_WAIT {
            let now = self.wire.now();
            self.endpoint.advance_clock(now);
            self.pump();
            self.flush();
            if !self.answered.is_empty() || self.pending.is_empty() {
                return std::mem::take(&mut self.answered);
            }
            if cap.is_some_and(|cap| now >= cap) {
                return Vec::new_in(Global);
            }
            let Some(next) = self.endpoint.next_deadline() else {
                self.fail_pending("the HTTP/3 core holds no deadline while reads wait on it (a bug in the core)");
                continue;
            };
            let until = cap.map_or(next, |cap| cap.min(next));
            // Copied out, so the clock is advanced before the core is handed them (the driver
            // contract's first step).
            let mut came = Vec::new_in(Global);
            self.wire.wait(until, &mut |from, bytes| {
                let mut copy = Vec::with_capacity_in(bytes.len(), Global);
                copy.extend_from_slice(bytes);
                came.push((from, copy));
            });
            let now = self.wire.now();
            self.endpoint.advance_clock(now);
            for (from, bytes) in came {
                self.endpoint.received(from, &bytes);
            }
        }
        self.fail_pending(&format!(
            "the HTTP/3 core made no progress in {TURNS_MOST_PER_WAIT} turns of one wait"
        ));
        std::mem::take(&mut self.answered)
    }
}

impl<W: Wire> Bucket for H3Bucket<W> {
    fn now_ms(&mut self) -> u64 {
        self.wire.now().as_millis() as u64
    }

    fn send(&mut self, asked: Vec<Asked>) {
        let now = self.wire.now();
        self.endpoint.advance_clock(now);
        for Asked { ticket, op, deadline_ms } in asked {
            self.counts.requests += 1;
            match self.ask(&op, Duration::from_millis(deadline_ms)) {
                Ok((request, want)) => {
                    let object = op.name().to_owned();
                    let body = Vec::new_in(Global);
                    self.pending.insert(request, Pending { ticket, object, want, head: None, body });
                }
                Err(why) => self.answered.push((ticket, Reply::Failed { why })),
            }
        }
        self.pump();
        self.flush();
    }

    fn in_flight(&self) -> usize {
        self.pending.len() + self.answered.len()
    }

    fn wait(&mut self) -> Vec<(Ticket, Reply)> {
        self.wait_until(None)
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}

/// A bucket that can say, without blocking, which of its asks are answered.
pub trait Ready: Bucket {
    /// Every answer ready now; none if none is.
    fn ready(&mut self) -> Vec<(Ticket, Reply)>;
}

/// One bucket over two transports: anonymous reads over HTTP/3, everything else over
/// HTTP/1.1 (`S`, the [`crate::http::HttpBucket`]). Its clock is `S`'s; an ask's deadline is
/// moved onto the wire's clock as it is routed there.
pub struct Routed<W: Wire, S: Ready> {
    h3: H3Bucket<W>,
    h1: S,
}

impl<W: Wire, S: Ready> Routed<W, S> {
    pub fn new(h3: H3Bucket<W>, h1: S) -> Routed<W, S> {
        Routed { h3, h1 }
    }

    /// The HTTP/3 side, for its tickets.
    pub fn h3(&mut self) -> &mut H3Bucket<W> {
        &mut self.h3
    }
}

impl<W: Wire, S: Ready> Bucket for Routed<W, S> {
    fn now_ms(&mut self) -> u64 {
        self.h1.now_ms()
    }

    fn send(&mut self, asked: Vec<Asked>) {
        let (h1_now, h3_now) = (self.h1.now_ms(), self.h3.now_ms());
        let (mut to_h3, mut to_h1) = (Vec::new_in(Global), Vec::new_in(Global));
        for mut it in asked {
            if anonymous(&it.op) {
                it.deadline_ms = it.deadline_ms.saturating_sub(h1_now) + h3_now;
                to_h3.push(it);
            } else {
                to_h1.push(it);
            }
        }
        if !to_h3.is_empty() {
            self.h3.send(to_h3);
        }
        if !to_h1.is_empty() {
            self.h1.send(to_h1);
        }
    }

    fn in_flight(&self) -> usize {
        self.h3.in_flight() + self.h1.in_flight()
    }

    fn wait(&mut self) -> Vec<(Ticket, Reply)> {
        // Bounded: each turn lasts MIXED_SLICE on the wire or ends with answers, and every ask
        // on either side is answered by its deadline.
        for _ in 0..TURNS_MOST_PER_WAIT {
            if self.h3.in_flight() == 0 {
                return self.h1.wait();
            }
            if self.h1.in_flight() == 0 {
                return self.h3.wait();
            }
            let mut out = self.h1.ready();
            let cap = self.h3.wire.now() + MIXED_SLICE;
            out.extend(self.h3.wait_until(Some(cap)));
            if !out.is_empty() {
                return out;
            }
        }
        let mut out = self.h3.wait();
        out.extend(self.h1.wait());
        out
    }

    fn counts(&self) -> Counts {
        let (a, b) = (self.h3.counts(), self.h1.counts());
        Counts { requests: a.requests + b.requests, sent: a.sent + b.sent, received: a.received + b.received }
    }
}

/// The tickets file's bytes for `tickets` (at most the [`TICKETS_KEPT`] newest, each within
/// the bounds [`decode_tickets`] holds a file to).
pub fn encode_tickets(tickets: &[h3::Ticket]) -> Vec<u8, Global> {
    let mut fit = Vec::with_capacity_in(tickets.len(), Global);
    fit.extend(
        tickets
            .iter()
            .filter(|it| it.server_name.len() <= TICKET_NAME_MOST && it.bytes.len() <= TICKET_BYTES_MOST),
    );
    let kept = &fit[fit.len().saturating_sub(TICKETS_KEPT)..];
    let mut out = Vec::new_in(Global);
    out.extend_from_slice(TICKETS_MAGIC);
    out.push(TICKETS_LAYOUT);
    out.push(kept.len() as u8);
    for ticket in kept {
        out.extend_from_slice(&(ticket.server_name.len() as u16).to_le_bytes());
        out.extend_from_slice(ticket.server_name.as_bytes());
        out.extend_from_slice(&ticket.issued_unix_s.to_le_bytes());
        out.extend_from_slice(&ticket.lifetime_s.to_le_bytes());
        out.extend_from_slice(&(ticket.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&ticket.bytes);
    }
    out
}

/// The most bytes a tickets file may be: its head and [`TICKETS_KEPT`] tickets at their
/// bounds. A driver reads no more than this.
pub const TICKETS_FILE_MOST: usize =
    TICKETS_MAGIC.len() + 2 + TICKETS_KEPT * (2 + TICKET_NAME_MOST + 8 + 4 + 4 + TICKET_BYTES_MOST);

/// The tickets in a tickets file's `bytes` still usable at `now_unix_s`: one past its age
/// ([`TICKET_AGE_MOST_S`], or its own lifetime if shorter) is left out, as the server would
/// refuse it. A file of another layout or out of bounds is refused, naming the file.
pub fn decode_tickets(bytes: &[u8], now_unix_s: u64) -> Result<Vec<h3::Ticket, Global>, String> {
    let bad = |why: &str| format!("{TICKETS_FILE}: {why}: delete it (it holds only resumption tickets)");
    if bytes.len() > TICKETS_FILE_MOST {
        return Err(bad(&format!("{} bytes, past the {TICKETS_FILE_MOST} a tickets file may be", bytes.len())));
    }
    let Some(rest) = bytes.strip_prefix(TICKETS_MAGIC.as_slice()) else {
        return Err(bad("not a tickets file"));
    };
    let mut at = Reader { bytes: rest };
    let layout = at.take(1).ok_or_else(|| bad("no layout byte"))?[0];
    if layout != TICKETS_LAYOUT {
        return Err(bad(&format!("layout {layout}, and this blaze reads layout {TICKETS_LAYOUT}")));
    }
    let count = usize::from(at.take(1).ok_or_else(|| bad("no count"))?[0]);
    if count > TICKETS_KEPT {
        return Err(bad(&format!("{count} tickets, past the {TICKETS_KEPT} kept")));
    }
    let mut out = Vec::with_capacity_in(count, Global);
    for i in 0..count {
        let short = || bad(&format!("ticket {i} is cut short"));
        let name_len = usize::from(at.u16().ok_or_else(short)?);
        if name_len > TICKET_NAME_MOST {
            return Err(bad(&format!("ticket {i}'s server name is {name_len} bytes, past {TICKET_NAME_MOST}")));
        }
        let name = std::str::from_utf8(at.take(name_len).ok_or_else(short)?)
            .map_err(|_| bad(&format!("ticket {i}'s server name is not UTF-8")))?
            .to_owned();
        let issued_unix_s = at.u64().ok_or_else(short)?;
        let lifetime_s = at.u32().ok_or_else(short)?;
        let len = at.u32().ok_or_else(short)? as usize;
        if len > TICKET_BYTES_MOST {
            return Err(bad(&format!("ticket {i} is {len} bytes, past {TICKET_BYTES_MOST}")));
        }
        let mut ticket = Vec::with_capacity_in(len, Global);
        ticket.extend_from_slice(at.take(len).ok_or_else(short)?);
        let age_most = TICKET_AGE_MOST_S.min(u64::from(lifetime_s));
        if now_unix_s < issued_unix_s.saturating_add(age_most) {
            out.push(h3::Ticket { server_name: name, issued_unix_s, lifetime_s, bytes: ticket });
        }
    }
    if !at.bytes.is_empty() {
        return Err(bad(&format!("{} bytes past its last ticket", at.bytes.len())));
    }
    Ok(out)
}

/// A cursor over a tickets file.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.bytes.split_at_checked(n)?;
        self.bytes = rest;
        Some(head)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
