//! [`Client`]: the HTTP/1.1 client as a synchronous state machine on an injected clock. It owns
//! no socket, runtime, thread or timer: a driver opens connections and moves bytes as [`Io`]
//! asks, tells it how each went ([`Completion`]) and what time it is
//! ([`Client::advance_clock`]), and hands its [`Event`]s to the askers. The driver contract is
//! in the crate's documentation.
//!
//! One request ([`Request`]) is one exchange: a connection to its origin (taken idle from the
//! pool, else a new one, with TLS for `https`), its head and body written, the answer's head
//! parsed, a redirect followed on another exchange of the same request (its connection reading
//! the redirect's body to its end in the background, to be kept), the final answer's body read
//! as its head frames it, decoded from gzip where the request asked for gzip, and held to the
//! request's bound. Then the connection goes back to the pool if it can take another request.
//!
//! Every wait is a deadline: the answer's head within the timeout of its exchange starting
//! (the connect, TLS and the head as one wait, as before), then each piece of the body within
//! the timeout of the last (not counting time the asker holds the body back), a redirect's
//! body within the timeout, and an idle connection within the pool's idle expiry.
//!
//! A connection goes through the proxy its [`Config::proxies`] names for its origin, if one
//! does ([`crate::proxy`]): an `https` origin, or any origin through SOCKS, through a tunnel
//! opened first ([`Handshake`]: the driver connects to the proxy, or resolves the origin's name
//! first for `socks4://` and `socks5://`), its TLS spoken inside the tunnel to the origin; an
//! `http` origin through an HTTP proxy by sending the proxy each request in absolute form. Its
//! pool key is still the origin: every connection to one origin goes the same way.
//!
//! A request sent on a connection taken from the pool, which closes before any byte of its
//! answer comes (the server closed it as it was taken), is sent once more on a new connection
//! when its method is idempotent (RFC 9110 section 9.2.2), as hyper-util retried a request its
//! pooled connection had closed under. Every other failure is the request's.

#![blaze::sans_io]

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use rustls::pki_types::ServerName;

use crate::{
    gzip::Gunzip,
    h1,
    pool::{Origin, Pool},
    proxy::{Credentials, Handshake, Proxies, ProxyKind},
    tls::Session,
};

/// How many redirects one request follows.
pub const REDIRECTS: usize = 10;
/// How long an idle connection is kept for another request: hyper-util's default.
pub const IDLE_FOR: Duration = Duration::from_secs(90);
/// The most idle connections the pool keeps.
pub const IDLE_MOST: usize = 64;
/// How many bytes of a body the client reads ahead of its asker ([`Client::consumed`]): past
/// it, it asks the connection for no more until the asker catches up.
pub const WINDOW: usize = 256 << 10;
/// The most bytes of a redirect's body read to keep its connection; past it the connection
/// closes.
const DRAIN_MOST: u64 = 64 << 10;

/// A request, named by its driver: unique among the requests in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// A connection, named by the client: never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// What every request of a [`Client`] shares.
#[derive(Clone, Debug)]
pub struct Config {
    /// The TLS of every `https` connection: its roots, versions and crypto. No ALPN is asked
    /// for: this client speaks HTTP/1.1 only.
    pub tls: Arc<rustls::ClientConfig>,
    pub user_agent: String,
    /// How long a connection or a transfer may go without a byte, cargo's `http.timeout`.
    pub timeout: Duration,
    /// How many redirects one request follows.
    pub redirects: usize,
    /// How long an idle connection is kept.
    pub idle_for: Duration,
    /// The most idle connections kept.
    pub idle_most: usize,
    /// The proxies connections go through, by their origin ([`crate::proxy_env::from_env`]
    /// reads the process's).
    pub proxies: Proxies,
}

impl Config {
    /// `tls`, `user_agent` and `timeout`, with [`REDIRECTS`], [`IDLE_FOR`], [`IDLE_MOST`] and
    /// no proxy.
    pub fn new(
        tls: Arc<rustls::ClientConfig>,
        user_agent: impl Into<String>,
        timeout: Duration,
    ) -> Config {
        Config {
            tls,
            user_agent: user_agent.into(),
            timeout,
            redirects: REDIRECTS,
            idle_for: IDLE_FOR,
            idle_most: IDLE_MOST,
            proxies: Proxies::none(),
        }
    }
}

/// One request: `method` of `url` with `headers` and `body`, after its redirects (each asked
/// with the same method, headers and body, as blaze-registry's client always asked them).
#[derive(Clone, Debug)]
pub struct Request {
    pub method: http::Method,
    pub url: String,
    /// Sent after the client's `host` (which a caller's `Host` replaces), `user-agent` and
    /// `accept-encoding`. `Content-Length`, `Transfer-Encoding` and `Connection` are the
    /// client's, and a request naming one is refused ([`Kind::Header`]).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether gzip is asked for (`Accept-Encoding: gzip`) and a gzip body decoded. Without it
    /// a body comes as it was sent, whatever its `Content-Encoding`.
    pub gzip: bool,
    /// The most bytes its body may be, as sent and once decoded: past it reading stops
    /// ([`Kind::TooBig`]).
    pub most: u64,
    /// Whether it is asked, and its redirects followed, over HTTPS only.
    pub https_only: bool,
}

impl Request {
    /// A `GET` of `url`, gzip asked for, with no bound.
    pub fn get(url: impl Into<String>) -> Request {
        Request {
            method: http::Method::GET,
            url: url.into(),
            // alloc: empty, allocates nothing.
            headers: Vec::new(),
            // alloc: empty, allocates nothing.
            body: Vec::new(),
            gzip: true,
            most: u64::MAX,
            https_only: false,
        }
    }
}

/// IO the client asks its driver for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Io {
    /// A TCP connection to `host` (a name to resolve, or an IP address without brackets) at
    /// `port`: [`Completion::Connected`] or [`Completion::ConnectFailed`].
    Connect { conn: ConnId, host: String, port: u16 },
    /// The addresses `host` (a name) resolves to, for a proxy that is given the origin by its
    /// address (`socks4://`, `socks5://`): [`Completion::Resolved`] or
    /// [`Completion::ResolveFailed`]. `conn` is the connection that will need them; its
    /// connect is asked after.
    Resolve { conn: ConnId, host: String },
    /// All of `bytes` written to `conn`: [`Completion::Sent`] or [`Completion::Broken`].
    Send { conn: ConnId, bytes: Vec<u8> },
    /// The next bytes `conn`'s peer sends: [`Completion::Received`] or [`Completion::Broken`].
    Recv { conn: ConnId },
    /// `conn` dropped, what is in flight on it abandoned; no completion.
    Close { conn: ConnId },
}

/// How an [`Io`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    Connected {
        conn: ConnId,
    },
    /// `why` as the system said it.
    ConnectFailed {
        conn: ConnId,
        why: String,
    },
    Sent {
        conn: ConnId,
    },
    /// What the name of an [`Io::Resolve`] resolves to, in the order the system gave them.
    Resolved {
        conn: ConnId,
        addresses: Vec<IpAddr>,
    },
    /// `why` as the system said it.
    ResolveFailed {
        conn: ConnId,
        why: String,
    },
    /// Empty when the peer closed the connection.
    Received {
        conn: ConnId,
        bytes: Vec<u8>,
    },
    /// A send or receive on `conn` failed: `why` as the system said it.
    Broken {
        conn: ConnId,
        why: String,
    },
}

/// What the client tells a request's asker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The final answer's head: `url` is the address that answered, after redirects; header
    /// names are in lower case.
    Head {
        request: RequestId,
        url: String,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// The body's next piece, decoded. The asker says it took it with [`Client::consumed`].
    Body {
        request: RequestId,
        bytes: Vec<u8>,
    },
    Done {
        request: RequestId,
    },
    Failed {
        request: RequestId,
        error: Error,
    },
}

impl Event {
    pub fn request(&self) -> RequestId {
        match self {
            Event::Head { request, .. }
            | Event::Body { request, .. }
            | Event::Done { request }
            | Event::Failed { request, .. } => *request,
        }
    }

    /// Whether it is the request's last: `Done` or `Failed`.
    pub fn ends(&self) -> bool {
        matches!(self, Event::Done { .. } | Event::Failed { .. })
    }
}

/// Why a request failed: `url` is the address it was asking then (the given one, or where a
/// redirect led), as given, credentials and all, so it is never part of the message; the asker
/// names it, redacted as it redacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub url: String,
    pub kind: Kind,
}

/// What went wrong. Only [`Kind::transient`] ones are worth asking again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Not an address this client asks: why.
    Url(String),
    /// A header it can't send: why.
    Header(String),
    /// Not `https://`, and the request is over HTTPS only.
    NotHttps,
    /// A redirect to `to`, which is not `https://`, and the request is over HTTPS only.
    Downgraded { to: String },
    /// More than `most` redirects.
    Redirects { most: usize },
    /// A redirect (`status`) with no `Location`.
    RedirectsNowhere { status: u16 },
    /// A body past `most`: as its `Content-Length` says (`said`), or as read (`decoded`: once
    /// its gzip was decoded), where reading stopped.
    TooBig { most: u64, said: Option<u64>, decoded: bool },
    /// No answer's head within `after` of asking `host:port`.
    NoAnswer { after: Duration, host: String, port: u16 },
    /// No byte of the body within `after` of the last.
    Stalled { after: Duration, host: String, port: u16 },
    /// `step` with `host:port` failed: `why`.
    Io { step: Step, host: String, port: u16, why: String },
    /// A body its head says is gzip, which is not.
    Gzip(String),
    /// Its [`RequestId`] is another request's, still in flight: the driver's mistake.
    InUse,
}

impl Kind {
    /// Whether asking again may get an answer: no answer came, or it was cut short or garbled
    /// on the way.
    pub fn transient(&self) -> bool {
        match self {
            Kind::NoAnswer { .. } | Kind::Stalled { .. } | Kind::Io { .. } | Kind::Gzip(_) => true,
            Kind::Url(_)
            | Kind::Header(_)
            | Kind::NotHttps
            | Kind::Downgraded { .. }
            | Kind::Redirects { .. }
            | Kind::RedirectsNowhere { .. }
            | Kind::TooBig { .. }
            | Kind::InUse => false,
        }
    }
}

/// The step of an exchange that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Connect,
    /// Through the proxy: connecting to it, or the tunnel's handshake.
    Proxy,
    Tls,
    Send,
    Head,
    Body,
}

impl Step {
    /// The step as a message says it, before `host:port`.
    pub fn says(self) -> &'static str {
        match self {
            Step::Connect => "connecting to",
            Step::Proxy => "through a proxy to",
            Step::Tls => "TLS with",
            Step::Send => "sending to",
            Step::Head => "reading the answer's head from",
            Step::Body => "reading the body from",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.says())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Url(why) => write!(f, "no address this client asks: {why}"),
            Kind::Header(why) => write!(f, "a header the client can't send: {why}"),
            Kind::NotHttps => {
                f.write_str("not an https:// address, and this is fetched over HTTPS only")
            }
            Kind::Downgraded { .. } => {
                f.write_str("it redirects off https://, and this is fetched over HTTPS only")
            }
            Kind::Redirects { most } => write!(f, "more than {most} redirects"),
            Kind::RedirectsNowhere { status } => {
                write!(f, "it redirects ({status}) nowhere: its answer has no Location")
            }
            Kind::TooBig { most, said: Some(said), .. } => {
                write!(
                    f,
                    "its body is {said} bytes as its Content-Length says, more than the {most} it may be"
                )
            }
            Kind::TooBig { most, said: None, decoded } => write!(
                f,
                "its body is more than the {most} bytes it may be{}, and reading it stopped there",
                if *decoded { " once its gzip is decoded" } else { "" }
            ),
            Kind::NoAnswer { after, host, port } => {
                write!(f, "no answer from {host}:{port} in {after:?}")
            }
            Kind::Stalled { after, host, port } => {
                write!(f, "the transfer from {host}:{port} stalled for {after:?}")
            }
            Kind::Io { step, host, port, why } => write!(f, "{step} {host}:{port}: {why}"),
            Kind::Gzip(why) => write!(f, "its body is not the gzip its head says: {why}"),
            Kind::InUse => f.write_str("its request id is another request's, still in flight"),
        }
    }
}

/// The kind alone: the address may hold credentials ([`Error`]).
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.kind, f)
    }
}

impl std::error::Error for Error {}

/// A request in flight.
struct Exchange {
    request: Request,
    /// The address asked now: the request's, or where its redirects led.
    url: url::Url,
    /// Redirects followed so far.
    hops: usize,
    /// The connection asked on now, if one is.
    conn: Option<ConnId>,
    /// The origin asked now, for the messages of a timeout.
    origin: Option<Origin>,
    /// Whether it was sent once more after its pooled connection closed under it.
    retried: bool,
    /// Whether its next exchange takes a new connection, not a pooled one (it is that retry).
    fresh: bool,
    /// Whether the final answer's head came.
    answered: bool,
    /// When it fails if nothing comes: none while the asker holds its body back.
    deadline: Option<Duration>,
    /// Bytes of the body handed to the asker that it has not said it took.
    unconsumed: usize,
    /// The body's bytes as sent, and once decoded.
    raw: u64,
    decoded: u64,
    gzip: Option<Gunzip>,
}

/// A connection.
struct Conn {
    origin: Origin,
    /// The proxy it goes through, as a message names it (`host:port`); `None` when direct.
    via: Option<String>,
    /// The tunnel through the proxy still being opened: until it is, the session's bytes wait.
    tunnel: Option<Handshake>,
    /// A plain session's bytes sent before the tunnel opened, held for it.
    held: Vec<u8>,
    session: Session,
    /// Whether the driver said it is open.
    connected: bool,
    /// Whether an [`Io::Send`] is in flight.
    sending: bool,
    /// Whether an [`Io::Recv`] is in flight.
    receiving: bool,
    /// Bytes to send once the send in flight is done.
    out: Vec<u8>,
    /// Plaintext received and not read yet.
    plain: Vec<u8>,
    /// Whether the peer closed it (or said it is done sending).
    eof: bool,
    work: Work,
    /// Whether the exchange on it now is not its first.
    reused: bool,
    /// Whether any byte of the answer to the exchange on it now came.
    got: bool,
}

/// What a connection is doing.
enum Work {
    /// In the pool.
    Idle,
    /// Asked by `request`, waiting for its answer's head.
    Head { request: RequestId },
    /// Reading `request`'s answer's body.
    Body { request: RequestId, body: h1::Body, reusable: bool },
    /// Reading a redirect's body to its end, for no one, to be kept if `reusable`; at most
    /// `left` bytes more, by `until`.
    Draining { body: h1::Body, reusable: bool, left: u64, until: Duration },
}

impl Conn {
    fn new(origin: Origin, session: Session) -> Conn {
        Conn {
            origin,
            via: None,
            tunnel: None,
            // alloc: empty, allocates nothing unless a plain request waits for a tunnel.
            held: Vec::new(),
            session,
            connected: false,
            sending: false,
            receiving: false,
            // alloc: empty until the connection's first bytes; one per connection after.
            out: Vec::new(),
            // alloc: empty until the connection's first bytes; one per connection after.
            plain: Vec::new(),
            eof: false,
            work: Work::Idle,
            reused: false,
            got: false,
        }
    }

    /// The request it is asked by, if one is.
    fn request(&self) -> Option<RequestId> {
        match self.work {
            Work::Head { request } | Work::Body { request, .. } => Some(request),
            Work::Idle | Work::Draining { .. } => None,
        }
    }
}

/// The HTTP/1.1 client core (the module's documentation).
pub struct Client {
    config: Config,
    /// The clock as the driver last said it.
    now: Duration,
    // BTreeMaps: a deadline's failures and closes go out in id order, the same every run.
    conns: BTreeMap<ConnId, Conn>,
    exchanges: BTreeMap<RequestId, Exchange>,
    pool: Pool,
    io: VecDeque<Io>,
    events: VecDeque<Event>,
    next_conn: u64,
}

impl Client {
    pub fn new(config: Config) -> Client {
        let pool = Pool::new(config.idle_most, config.idle_for);
        Client {
            config,
            now: Duration::ZERO,
            conns: BTreeMap::new(),
            exchanges: BTreeMap::new(),
            pool,
            // alloc: the client's queues, made once with it.
            io: VecDeque::new(),
            // alloc: the client's queues, made once with it.
            events: VecDeque::new(),
            next_conn: 0,
        }
    }

    /// The time is `now`, since the driver started; it never goes back. What was due by then
    /// happens: requests past their deadline fail, redirects' connections still draining and
    /// idle connections past their expiry close.
    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
        let now = self.now;
        let late: Vec<RequestId> = self
            .exchanges
            .iter()
            .filter(|(_, it)| it.deadline.is_some_and(|due| due <= now))
            .map(|(id, _)| *id)
            .collect(); // alloc: the requests due now, usually none.
        for request in late {
            let Some(exchange) = self.exchanges.get(&request) else { continue };
            let after = self.config.timeout;
            let (host, port) = match &exchange.origin {
                // alloc: an error's message, made only when it fails.
                Some(origin) => (origin.host.clone(), origin.port),
                None => (String::new(), 0),
            };
            let kind = if exchange.answered {
                Kind::Stalled { after, host, port }
            } else {
                Kind::NoAnswer { after, host, port }
            };
            self.fail(request, kind);
        }
        let drained: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, it)| matches!(it.work, Work::Draining { until, .. } if until <= now))
            .map(|(id, _)| *id)
            .collect(); // alloc: the connections due now, usually none.
        for conn in drained {
            self.close(conn);
        }
        self.expire_idle();
    }

    /// When something is next due: a request's deadline, a redirect's drain, an idle
    /// connection's expiry. The driver calls [`Client::advance_clock`] then.
    pub fn next_deadline(&self) -> Option<Duration> {
        let requests = self.exchanges.values().filter_map(|it| it.deadline);
        let drains = self.conns.values().filter_map(|it| match it.work {
            Work::Draining { until, .. } => Some(until),
            Work::Idle | Work::Head { .. } | Work::Body { .. } => None,
        });
        requests.chain(drains).chain(self.pool.next_expiry()).min()
    }

    /// The next IO to do, in the order asked.
    pub fn next_io(&mut self) -> Option<Io> {
        self.io.pop_front()
    }

    /// The next event for an asker, in the order they happened.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Starts `request` as `id`. A request that can't be asked (its address, a header, HTTPS
    /// only) fails at once, as an [`Event::Failed`]. An `id` already in flight is refused here,
    /// changing nothing: its events would go to the other request's asker.
    pub fn request(&mut self, id: RequestId, request: Request) -> Result<(), Error> {
        if self.exchanges.contains_key(&id) {
            return Err(Error { url: request.url, kind: Kind::InUse });
        }
        let url = match url::Url::parse(&request.url) {
            Ok(url) => url,
            Err(e) => {
                // alloc: an error's message, made only when it fails.
                let kind = Kind::Url(format!("it does not parse as a URL: {e}"));
                self.events.push_back(Event::Failed {
                    request: id,
                    error: Error { url: request.url, kind },
                });
                return Ok(());
            }
        };
        let refused = if request.https_only && url.scheme() != "https" {
            Some(Kind::NotHttps)
        } else {
            request
                .headers
                .iter()
                .find_map(|(name, value)| h1::refuse_header(name, value))
                .map(Kind::Header)
        };
        if let Some(kind) = refused {
            self.events
                .push_back(Event::Failed { request: id, error: Error { url: request.url, kind } });
            return Ok(());
        }
        let exchange = Exchange {
            request,
            url,
            hops: 0,
            conn: None,
            origin: None,
            retried: false,
            fresh: false,
            answered: false,
            deadline: None,
            unconsumed: 0,
            raw: 0,
            decoded: 0,
            gzip: None,
        };
        self.exchanges.insert(id, exchange);
        self.start(id);
        Ok(())
    }

    /// The asker of `request` took `bytes` more of its body: the client reads on once it is
    /// less than [`WINDOW`] ahead.
    pub fn consumed(&mut self, request: RequestId, bytes: usize) {
        let now = self.now;
        let timeout = self.config.timeout;
        let Some(exchange) = self.exchanges.get_mut(&request) else { return };
        let held = exchange.unconsumed >= WINDOW;
        exchange.unconsumed = exchange.unconsumed.saturating_sub(bytes);
        let Some(conn) = exchange.conn else { return };
        if held && exchange.unconsumed < WINDOW {
            // The stall's clock starts again: the time held back was the asker's.
            exchange.deadline = Some(now + timeout);
            self.advance(conn);
            self.pump(conn);
        }
    }

    /// The asker of `request` went away: it ends with no more events, and its connection, if
    /// it was reading its answer, closes.
    pub fn cancel(&mut self, request: RequestId) {
        let Some(exchange) = self.exchanges.remove(&request) else { return };
        self.drop_conn_of(request, exchange.conn);
    }

    /// How an [`Io`] went.
    pub fn complete(&mut self, completion: Completion) {
        match completion {
            Completion::Connected { conn } => {
                let Some(it) = self.conns.get_mut(&conn) else { return };
                it.connected = true;
                self.pump(conn);
            }
            Completion::ConnectFailed { conn, why } => {
                let Some(it) = self.conns.get(&conn) else { return };
                let (step, why) = match &it.via {
                    // alloc: an error's message, made only when it fails.
                    Some(via) => (Step::Proxy, format!("connecting to the proxy {via}: {why}")),
                    None => (Step::Connect, why),
                };
                self.conn_failed(conn, step, why);
            }
            Completion::Resolved { conn, addresses } => {
                let Some(it) = self.conns.get_mut(&conn) else { return };
                let Some(tunnel) = it.tunnel.as_mut() else { return };
                if let Err(why) = tunnel.resolved(&addresses) {
                    return self.conn_failed(conn, Step::Proxy, why);
                }
                let proxy = tunnel.proxy();
                // alloc: one per connection, the proxy's host the driver resolves.
                let host = proxy.host.clone();
                self.io.push_back(Io::Connect { conn, host, port: proxy.port });
            }
            Completion::ResolveFailed { conn, why } => {
                let Some(it) = self.conns.get(&conn) else { return };
                // alloc: an error's message, made only when it fails.
                let why = format!(
                    "resolving {}, which the proxy {} is given by its address: {why}",
                    it.origin.host,
                    it.via.as_deref().unwrap_or_default()
                );
                self.conn_failed(conn, Step::Proxy, why);
            }
            Completion::Sent { conn } => {
                let Some(it) = self.conns.get_mut(&conn) else { return };
                it.sending = false;
                self.pump(conn);
            }
            Completion::Received { conn, bytes } => self.received(conn, &bytes),
            Completion::Broken { conn, why } => {
                let Some(it) = self.conns.get(&conn) else { return };
                let step = if it.tunnel.is_some() {
                    Step::Proxy
                } else if it.session.handshaking() {
                    Step::Tls
                } else {
                    match it.work {
                        Work::Head { .. } if it.sending => Step::Send,
                        Work::Body { .. } => Step::Body,
                        Work::Head { .. } | Work::Idle | Work::Draining { .. } => Step::Head,
                    }
                };
                self.conn_failed(conn, step, why);
            }
        }
    }

    /// Starts the exchange `id` is on now: a connection to its origin, its head and body
    /// written to it.
    fn start(&mut self, id: RequestId) {
        self.expire_idle();
        let Some(exchange) = self.exchanges.get(&id) else { return };
        let origin = match origin_of(&exchange.url) {
            Ok(origin) => origin,
            Err(kind) => return self.fail(id, kind),
        };
        let fresh = exchange.fresh;
        // The proxy every connection to this origin goes through: the config's, which never
        // changes, so a pooled connection went the way a new one would.
        let proxy = self.config.proxies.of(origin.https, &origin.host);
        // An `http` origin through an HTTP proxy: each request to the proxy, in absolute form.
        let forward = proxy.filter(|it| !origin.https && it.kind == ProxyKind::Http);
        let target = match forward {
            Some(_) => absolute_of(&exchange.url),
            None => target_of(&exchange.url),
        };
        // alloc: one per exchange through a proxy with a user, its header's value.
        let proxy_authorization = forward.and_then(|it| it.auth.as_ref()).map(Credentials::basic);
        // alloc: one per exchange, its head and body as written.
        let mut written = Vec::with_capacity(512 + exchange.request.body.len());
        h1::write_request(
            &mut written,
            &exchange.request.method,
            &target,
            &host_of(&exchange.url),
            &self.config.user_agent,
            exchange.request.gzip,
            &exchange.request.headers,
            exchange.request.body.len(),
            proxy_authorization.as_deref(),
        );
        written.extend_from_slice(&exchange.request.body);
        let pooled = if fresh { None } else { self.pool.take(&origin) };
        let conn = match pooled {
            Some(conn) => conn,
            None => {
                let session = if origin.https {
                    // alloc: one per connection, the name its certificate is checked against.
                    let name = match ServerName::try_from(origin.host.clone()) {
                        Ok(name) => name,
                        Err(e) => {
                            // alloc: an error's message, made only when it fails.
                            let why = format!(
                                "`{}` is no name a TLS certificate can be checked against: {e}",
                                origin.host
                            );
                            return self.fail(id, Kind::Url(why));
                        }
                    };
                    match Session::tls(&self.config.tls, name) {
                        Ok(session) => session,
                        Err(why) => {
                            let (host, port) = (origin.host, origin.port);
                            return self.fail(id, Kind::Io { step: Step::Tls, host, port, why });
                        }
                    }
                } else {
                    Session::Plain
                };
                let conn = ConnId(self.next_conn);
                self.next_conn += 1;
                // alloc: one per connection, its origin.
                let mut made = Conn::new(origin.clone(), session);
                match proxy {
                    None => {
                        // alloc: one per connection, the host the driver resolves.
                        let host = origin.host.clone();
                        self.io.push_back(Io::Connect { conn, host, port: origin.port });
                    }
                    Some(proxy) => {
                        if forward.is_none() {
                            let tunnel = match Handshake::new(proxy, &origin.host, origin.port) {
                                Ok(tunnel) => tunnel,
                                Err(why) => {
                                    let (host, port) = (origin.host, origin.port);
                                    let kind = Kind::Io { step: Step::Proxy, host, port, why };
                                    return self.fail(id, kind);
                                }
                            };
                            if tunnel.resolving() {
                                // alloc: one per connection, the origin's name the driver resolves.
                                let host = origin.host.clone();
                                self.io.push_back(Io::Resolve { conn, host });
                            }
                            made.tunnel = Some(tunnel);
                        }
                        // A tunnel that waits for the origin's addresses connects once they come.
                        if !made.tunnel.as_ref().is_some_and(Handshake::resolving) {
                            // alloc: one per connection, the proxy's host the driver resolves.
                            let host = proxy.host.clone();
                            self.io.push_back(Io::Connect { conn, host, port: proxy.port });
                        }
                        made.via = Some(proxy.at());
                    }
                }
                self.conns.insert(conn, made);
                conn
            }
        };
        let deadline = self.now + self.config.timeout;
        if let Some(exchange) = self.exchanges.get_mut(&id) {
            exchange.conn = Some(conn);
            exchange.origin = Some(origin);
            exchange.fresh = false;
            exchange.answered = false;
            exchange.deadline = Some(deadline);
        }
        let Some(it) = self.conns.get_mut(&conn) else { return };
        it.work = Work::Head { request: id };
        it.reused = pooled.is_some();
        it.got = false;
        let Conn { session, out, held, tunnel, .. } = it;
        // A tunnel still opening holds a plain request's bytes until it is open (TLS holds its
        // own, its handshake not begun).
        let into = if tunnel.is_some() { held } else { out };
        if let Err(why) = session.send(&written, into) {
            return self.conn_failed(conn, Step::Send, why);
        }
        self.pump(conn);
    }

    /// Asks the driver for what `id` needs next: a send of what is waiting, when none is in
    /// flight, and a receive, when none is and someone wants the bytes (an exchange that has
    /// room for them, a redirect's drain, or the pool, to hear a close).
    fn pump(&mut self, id: ConnId) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if !conn.connected {
            return;
        }
        if !conn.sending {
            // Through a tunnel still opening, only its handshake goes: the session waits.
            let made = match conn.tunnel.as_mut() {
                Some(tunnel) => {
                    tunnel.outgoing(&mut conn.out);
                    Ok(())
                }
                None => conn.session.outgoing(&mut conn.out),
            };
            if let Err(why) = made {
                let step = if conn.session.handshaking() { Step::Tls } else { Step::Send };
                return self.conn_failed(id, step, why);
            }
            if !conn.out.is_empty() {
                conn.sending = true;
                // alloc: the bytes handed to the driver; the next are gathered in a new buffer.
                let bytes = std::mem::take(&mut conn.out);
                self.io.push_back(Io::Send { conn: id, bytes });
            }
        }
        let wants = match &conn.work {
            Work::Idle | Work::Head { .. } | Work::Draining { .. } => true,
            Work::Body { request, .. } => {
                self.exchanges.get(request).is_some_and(|it| it.unconsumed < WINDOW)
            }
        };
        if wants && !conn.receiving && !conn.eof {
            conn.receiving = true;
            self.io.push_back(Io::Recv { conn: id });
        }
    }

    fn received(&mut self, id: ConnId, bytes: &[u8]) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.receiving = false;
        // What came after a tunnel's handshake, the origin's first bytes.
        let mut after = None;
        if let Some(tunnel) = conn.tunnel.as_mut() {
            let opened =
                if bytes.is_empty() { Err(tunnel.closed()) } else { tunnel.received(bytes) };
            match opened {
                Ok(None) => return self.pump(id),
                Ok(Some(rest)) => {
                    conn.tunnel = None;
                    conn.out.append(&mut conn.held);
                    if rest.is_empty() {
                        return self.pump(id);
                    }
                    after = Some(rest);
                }
                Err(why) => return self.conn_failed(id, Step::Proxy, why),
            }
        }
        let bytes = after.as_deref().unwrap_or(bytes);
        if bytes.is_empty() {
            conn.eof = true;
        } else {
            let before = conn.plain.len();
            match conn.session.received(bytes, &mut conn.plain) {
                Ok(closed) => conn.eof |= closed,
                Err(why) => {
                    let step = match conn.work {
                        _ if conn.session.handshaking() => Step::Tls,
                        Work::Body { .. } => Step::Body,
                        Work::Idle | Work::Head { .. } | Work::Draining { .. } => Step::Head,
                    };
                    return self.conn_failed(id, step, why);
                }
            }
            conn.got |= conn.plain.len() > before;
        }
        self.advance(id);
        self.pump(id);
    }

    /// Reads what `id` has received as its work says, as far as it goes.
    fn advance(&mut self, id: ConnId) {
        loop {
            let Some(conn) = self.conns.get_mut(&id) else { return };
            match &mut conn.work {
                Work::Idle => {
                    // An idle connection is sent nothing: bytes or a close end it.
                    if conn.eof || !conn.plain.is_empty() {
                        self.close(id);
                    }
                    return;
                }
                Work::Draining { body, reusable, left, .. } => {
                    // alloc: a redirect's body, read to be dropped, at most `DRAIN_MOST`.
                    let mut dropped = Vec::new();
                    let Ok((used, done)) = body.decode(&conn.plain, &mut dropped) else {
                        return self.close(id);
                    };
                    conn.plain.drain(..used);
                    let read = dropped.len() as u64;
                    if read > *left {
                        return self.close(id);
                    }
                    *left -= read;
                    if done && *reusable && conn.plain.is_empty() && !conn.eof {
                        return self.idle(id);
                    }
                    if done || conn.eof {
                        self.close(id);
                    }
                    return;
                }
                Work::Head { request } => {
                    let request = *request;
                    let head_request = self
                        .exchanges
                        .get(&request)
                        .is_some_and(|it| it.request.method == http::Method::HEAD);
                    let head = match h1::parse_head(&conn.plain, head_request) {
                        Ok(Some(head)) => head,
                        Ok(None) => {
                            if conn.eof {
                                let why = if conn.plain.is_empty() {
                                    "the connection closed before any answer came"
                                } else {
                                    "the connection closed in the middle of the answer's head"
                                };
                                self.conn_failed(id, Step::Head, why.to_owned());
                            }
                            return;
                        }
                        Err(why) => return self.conn_failed(id, Step::Head, why),
                    };
                    conn.plain.drain(..head.len);
                    if (100..200).contains(&head.status) {
                        if head.status == 101 {
                            let why = "101 Switching Protocols, which was not asked for".to_owned();
                            return self.conn_failed(id, Step::Head, why);
                        }
                        // An informational answer (100 Continue, 103 Early Hints): the real one follows.
                        continue;
                    }
                    self.answer(id, request, head);
                }
                Work::Body { request, body, reusable } => {
                    let request = *request;
                    let reusable = *reusable;
                    let Some(exchange) = self.exchanges.get_mut(&request) else {
                        return self.close(id);
                    };
                    if exchange.unconsumed >= WINDOW {
                        exchange.deadline = None;
                        return;
                    }
                    // alloc: one per piece of the body, handed to the asker.
                    let mut data = Vec::new();
                    let (used, done) = match body.decode(&conn.plain, &mut data) {
                        Ok(it) => it,
                        Err(why) => return self.conn_failed(id, Step::Body, why),
                    };
                    conn.plain.drain(..used);
                    if used > 0 {
                        exchange.deadline = Some(self.now + self.config.timeout);
                    }
                    if let Err(kind) = deliver(exchange, data, &mut self.events, request) {
                        return self.fail(request, kind);
                    }
                    if done {
                        let keep = reusable && conn.plain.is_empty() && !conn.eof;
                        return self.finish(request, id, keep);
                    }
                    if conn.eof {
                        return match body.closed() {
                            Ok(()) => self.finish(request, id, false),
                            Err(why) => self.conn_failed(id, Step::Body, why),
                        };
                    }
                    return;
                }
            }
        }
    }

    /// `head` answered `request` on `conn`: a redirect starts its next exchange, leaving
    /// `conn` to read the redirect's body; the final answer is told to the asker.
    fn answer(&mut self, conn: ConnId, request: RequestId, head: h1::Head) {
        let h1::Head { status, headers, body, reusable, location, gzip, length, .. } = head;
        let now = self.now;
        let timeout = self.config.timeout;
        let redirects = self.config.redirects;
        let Some(exchange) = self.exchanges.get_mut(&request) else { return self.close(conn) };
        if (300..400).contains(&status) && status != 304 {
            let Some(location) = location else {
                return self.fail(request, Kind::RedirectsNowhere { status });
            };
            let next = match redirect_to(&exchange.url, &location, exchange.request.https_only) {
                Ok(next) => next,
                Err(kind) => return self.fail(request, kind),
            };
            exchange.hops += 1;
            if exchange.hops > redirects {
                return self.fail(request, Kind::Redirects { most: redirects });
            }
            exchange.url = next;
            exchange.conn = None;
            if let Some(it) = self.conns.get_mut(&conn) {
                it.work = Work::Draining { body, reusable, left: DRAIN_MOST, until: now + timeout };
            }
            // The redirect's body first: when it is all here already (a short or empty one),
            // its connection is back in the pool for the next exchange to take, if that goes to
            // the same origin.
            self.advance(conn);
            return self.start(request);
        }
        let most = exchange.request.most;
        if let Some(length) = length.filter(|it| *it > most) {
            return self.fail(request, Kind::TooBig { most, said: Some(length), decoded: false });
        }
        if gzip && exchange.request.gzip {
            exchange.gzip = Some(Gunzip::new());
        }
        exchange.answered = true;
        exchange.deadline = Some(now + timeout);
        // alloc: one per answer, the address that answered, for the asker.
        let url = exchange.url.to_string();
        self.events.push_back(Event::Head { request, url, status, headers });
        if let Some(it) = self.conns.get_mut(&conn) {
            it.work = Work::Body { request, body, reusable };
        }
    }

    /// `request`'s body ended whole on `conn`: the rest of its gzip, then `Done`, and `conn`
    /// back to the pool if `keep`.
    fn finish(&mut self, request: RequestId, conn: ConnId, keep: bool) {
        let Some(exchange) = self.exchanges.get_mut(&request) else { return };
        let ended = match exchange.gzip.as_mut().map(Gunzip::finish) {
            None => Ok(()),
            Some(Ok(rest)) => give(exchange, rest, &mut self.events, request),
            Some(Err(why)) => Err(Kind::Gzip(why)),
        };
        if let Err(kind) = ended {
            return self.fail(request, kind);
        }
        self.exchanges.remove(&request);
        self.events.push_back(Event::Done { request });
        if keep { self.idle(conn) } else { self.close(conn) }
    }

    /// `id` into the pool, idle from now, with a receive asked of it to hear a close.
    fn idle(&mut self, id: ConnId) {
        let now = self.now;
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.work = Work::Idle;
        // alloc: one per connection kept, its key in the pool.
        let origin = conn.origin.clone();
        if let Some(gone) = self.pool.put(origin, id, now) {
            self.close(gone);
        }
        self.pump(id);
    }

    /// `id` closed, and the driver told to drop it. Whoever was asking on it is told by the
    /// caller.
    fn close(&mut self, id: ConnId) {
        if self.conns.remove(&id).is_some() {
            self.pool.remove(id);
            self.io.push_back(Io::Close { conn: id });
        }
    }

    /// Idle connections past their expiry, closed.
    fn expire_idle(&mut self) {
        // alloc: empty unless a connection expired.
        let mut gone = Vec::new();
        self.pool.expire(self.now, &mut gone);
        for conn in gone {
            self.close(conn);
        }
    }

    /// `step` failed on `id` with `why`: it closes, and the request asking on it fails, or is
    /// sent once more on a new connection (the module's documentation).
    fn conn_failed(&mut self, id: ConnId, step: Step, why: String) {
        let Some(conn) = self.conns.get(&id) else { return };
        let request = conn.request();
        let retry = matches!(conn.work, Work::Head { .. }) && conn.reused && !conn.got;
        // alloc: an error's message, made only when it fails.
        let (host, port) = (conn.origin.host.clone(), conn.origin.port);
        self.close(id);
        let Some(request) = request else { return };
        let Some(exchange) = self.exchanges.get_mut(&request) else { return };
        exchange.conn = None;
        if retry && !exchange.retried && idempotent(&exchange.request.method) {
            exchange.retried = true;
            exchange.fresh = true;
            return self.start(request);
        }
        self.fail(request, Kind::Io { step, host, port, why });
    }

    /// `request` fails with `kind`: its connection, if it was asking on one, closes.
    fn fail(&mut self, request: RequestId, kind: Kind) {
        let Some(exchange) = self.exchanges.remove(&request) else { return };
        self.drop_conn_of(request, exchange.conn);
        let error = Error { url: exchange.url.into(), kind };
        self.events.push_back(Event::Failed { request, error });
    }

    /// Closes `conn` if `request` is asking on it.
    fn drop_conn_of(&mut self, request: RequestId, conn: Option<ConnId>) {
        let Some(conn) = conn else { return };
        if self.conns.get(&conn).and_then(Conn::request) == Some(request) {
            self.close(conn);
        }
    }
}

/// `data`, the body's next bytes as sent, to the asker of `request`: decoded if it is gzip,
/// held to the request's bound.
fn deliver(
    exchange: &mut Exchange,
    data: Vec<u8>,
    events: &mut VecDeque<Event>,
    request: RequestId,
) -> Result<(), Kind> {
    if data.is_empty() {
        return Ok(());
    }
    let most = exchange.request.most;
    exchange.raw += data.len() as u64;
    if exchange.raw > most {
        return Err(Kind::TooBig { most, said: None, decoded: false });
    }
    let bytes = match exchange.gzip.as_mut() {
        Some(gzip) => gzip.feed(&data).map_err(Kind::Gzip)?,
        None => data,
    };
    give(exchange, bytes, events, request)
}

/// `bytes`, decoded, to the asker of `request` as an [`Event::Body`]; a gzip body's decoded
/// bytes held to the bound. A full window stops the stall's clock until the asker takes some.
fn give(
    exchange: &mut Exchange,
    bytes: Vec<u8>,
    events: &mut VecDeque<Event>,
    request: RequestId,
) -> Result<(), Kind> {
    if bytes.is_empty() {
        return Ok(());
    }
    if exchange.gzip.is_some() {
        let most = exchange.request.most;
        exchange.decoded += bytes.len() as u64;
        if exchange.decoded > most {
            return Err(Kind::TooBig { most, said: None, decoded: true });
        }
    }
    exchange.unconsumed += bytes.len();
    if exchange.unconsumed >= WINDOW {
        exchange.deadline = None;
    }
    events.push_back(Event::Body { request, bytes });
    Ok(())
}

/// Where `from` redirecting to `location` (its `Location`, maybe relative) leads; with
/// `https_only`, a redirect off HTTPS is [`Kind::Downgraded`].
pub(crate) fn redirect_to(
    from: &url::Url,
    location: &str,
    https_only: bool,
) -> Result<url::Url, Kind> {
    // alloc: an error's message, made only when it fails.
    let next = from
        .join(location)
        .map_err(|e| Kind::Url(format!("its redirect leads to no address: {e}")))?;
    if https_only && next.scheme() != "https" {
        return Err(Kind::Downgraded { to: next.into() });
    }
    Ok(next)
}

/// The origin `url` is asked of: `http` or `https` only, its port the scheme's when it names
/// none, an IPv6 host without its brackets.
fn origin_of(url: &url::Url) -> Result<Origin, Kind> {
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        // alloc: an error's message, made only when it fails.
        other => {
            return Err(Kind::Url(format!(
                "its scheme `{other}` is neither http nor https, the only ones this client speaks"
            )));
        }
    };
    // alloc: one per exchange, the host its connection goes to.
    let host = match url.host() {
        Some(url::Host::Domain(it)) => it.to_owned(),
        Some(url::Host::Ipv4(it)) => it.to_string(),
        Some(url::Host::Ipv6(it)) => it.to_string(),
        None => return Err(Kind::Url("it names no host".to_owned())),
    };
    let port = url.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
    Ok(Origin { https, host, port })
}

/// `url`'s `Host`: its host as written (an IPv6 one in brackets), with its port when that is
/// not the scheme's.
fn host_of(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        // alloc: one per exchange, its `Host` header.
        Some(port) => format!("{host}:{port}"),
        // alloc: one per exchange, its `Host` header.
        None => host.to_owned(),
    }
}

/// `url`'s request target in absolute form, as a proxy is asked (RFC 9112 section 3.2.2): its
/// scheme, host and port as [`host_of`] says them, path and query; no credentials and no
/// fragment.
fn absolute_of(url: &url::Url) -> String {
    // alloc: one per exchange through a proxy, its request target.
    format!("{}://{}{}", url.scheme(), host_of(url), target_of(url))
}

/// `url`'s request target in origin form: its path and query, no fragment.
fn target_of(url: &url::Url) -> String {
    match url.query() {
        // alloc: one per exchange, its request target.
        Some(query) => format!("{}?{query}", url.path()),
        // alloc: one per exchange, its request target.
        None => url.path().to_owned(),
    }
}

/// Whether sending `method` twice does what sending it once does (RFC 9110 section 9.2.2).
fn idempotent(method: &http::Method) -> bool {
    matches!(
        *method,
        http::Method::GET
            | http::Method::HEAD
            | http::Method::OPTIONS
            | http::Method::TRACE
            | http::Method::PUT
            | http::Method::DELETE
    )
}
