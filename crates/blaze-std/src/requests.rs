//! Making requests (docs/v2/plan/design/std.md, "Capabilities are layered, and each has
//! providers"): [`Http`], "may make requests to these origins". One type with three providers,
//! chosen where it is made, so `fn sync(cx: &Cx, http: &Http)` runs natively, in a worker and
//! in a test unchanged:
//!
//! | provider | how it answers a request |
//! |---|---|
//! | [`Http::over`] | bstd's sans-IO HTTP/1.1 core ([`crate::http::h1`]) over a connection from that [`Net`]; never reaches further than the `Net` does |
//! | [`crate::Io::http`] | the host does it; today the simulation's routes ([`SimHttp`]) alone |
//! | [`Http::handler`] | a function you pass: a fake for a test, a cache, a proxy |
//!
//! [`Http::only`] narrows whichever it is to one [`Origin`], and never widens.
//!
//! **What `over` does and refuses.** It drives blaze-http's `h1` core (the request writer, the
//! head parser and the body decoder, bytes in and bytes out) with a thin driver over a
//! [`Conn`]: one connection a request, closed when its answer is read; no pool, no redirects
//! and no gzip, which blaze-http's `Client` core adds. `https://` is refused
//! ([`HttpKind::NoTls`]): the only TLS in the tree is inside blaze-http's `Client`, over a
//! rustls configuration, and bstd has no sans-IO TLS core that takes a [`Roots`] yet. The
//! `Roots` handed in is kept for that day and trusted for nothing until then. A host that is a
//! name is refused by the `Net` ([`crate::net::NetKind::NoLookup`]) until blaze-core can look
//! one up.
//!
//! **Finishing.** A request in flight is an [`Exchange`], linear: [`Http::begin`] sends the
//! request and [`Exchange::finish`] reads its answer and closes its connection, so neither a
//! connection nor an answer's error can be dropped unseen. [`Http::request`] is the two
//! together.

use alloc::{alloc::Global, string::String, sync::Arc, vec::Vec};
use core::fmt;

use blaze_http::h1;

use crate::{
    Plant, Stopped,
    clock::Clock,
    cx::Cx,
    io::Io,
    mem::{self as bmem, Bytes, Named, OutOfMemory, heap},
    name::Excerpt,
    narrow::Allowed,
    net::{Addr, Conn, Net, NetError},
    roots::Roots,
    url::{Origin, Url},
};

/// How [`Http::over`] talks on the wire and where it stops: what it calls itself and the
/// two bounds of reading an answer. Each is its caller's to decide, so each is a field with
/// no default and [`Http::over`] takes the whole value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wire {
    /// The `User-Agent` of every request.
    pub user_agent: &'static str,
    /// The most bytes of an answer's body that are read: past it the request fails
    /// ([`HttpKind::TooBig`]). The bytes go to the caller's [`Bytes`], whose arena has its
    /// own limit; this one is the peer's.
    #[blaze::policy]
    pub body_most: usize,
    /// The most receives made for one answer before it stops waiting for its end
    /// ([`HttpKind::Unending`]; rule bounded-progress).
    #[blaze::policy]
    pub reads_most: usize,
}

/// The most bytes one receive of [`Http::over`] asks for.
const RECV: usize = 16 << 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn of_core(self) -> ::http::Method {
        match self {
            Method::Get => ::http::Method::GET,
            Method::Head => ::http::Method::HEAD,
            Method::Post => ::http::Method::POST,
            Method::Put => ::http::Method::PUT,
            Method::Patch => ::http::Method::PATCH,
            Method::Delete => ::http::Method::DELETE,
        }
    }
}

/// A request header, checked as it is made: a name that is a token, a value with no line
/// break, and not one of the headers that frame the body (`Content-Length`,
/// `Transfer-Encoding`, `Connection`), which are the client's to say.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: refcounts
pub struct Header {
    name: Arc<String>,
    value: Arc<String>,
}

/// What [`Header::new`] refused: the header's name and why, or that it could not be kept.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum HeaderError {
    /// The header is none: its name (its start when it is long, kept in the error, which
    /// allocates nothing), and why, as blaze-http's h1 says.
    Refused { name: Excerpt, why: String },
    /// The global heap had no room to keep the header, or its error's name.
    Stopped(Stopped),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::Refused { name, why } => write!(f, "header {name:?}: {why}"),
            HeaderError::Stopped(stopped) => write!(f, "a header could not be kept: {stopped}"),
        }
    }
}

impl From<OutOfMemory> for HeaderError {
    fn from(full: OutOfMemory) -> HeaderError {
        HeaderError::Stopped(Stopped::from(full))
    }
}

impl core::error::Error for HeaderError {}

impl Header {
    pub fn new(name: &str, value: &str) -> Result<Header, HeaderError> {
        if let Some(why) = h1::refuse_header(name, value) {
            return Err(HeaderError::Refused { name: Excerpt::of(name), why });
        }
        // alloc: once per header, its name and value, and the handles their clones share.
        let (name, value) = (heap::shared(heap::string(name)?)?, heap::shared(heap::string(value)?)?);
        Ok(Header { name, value })
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn value(&self) -> &str {
        self.value.as_str()
    }
}

/// One request: `method` of `url` with `headers` and `body`.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub url: &'a Url,
    pub headers: &'a [Header],
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// A `GET` of `url` with no headers.
    pub fn get(url: &'a Url) -> Request<'a> {
        Request { method: Method::Get, url, headers: &[], body: &[] }
    }
}

/// An answer: its status, and how many bytes of body were added to the caller's bytes. Its
/// headers are not handed back yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Response {
    pub status: u16,
    pub body: usize,
}

/// What a request call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpOp {
    /// [`Http::begin`]: the request sent.
    Begin,
    /// [`Exchange::finish`]: the answer read.
    Finish,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum HttpKind {
    /// The capability does not reach the URL's origin (narrowing never widens).
    NotAllowed,
    /// The URL names no host to ask.
    NoOrigin,
    /// The URL's scheme is neither `http` nor `https`.
    NotHttp,
    /// `https://` through [`Http::over`]: bstd has no sans-IO TLS core that takes a [`Roots`]
    /// yet (see the module's doc).
    NoTls,
    /// This world has no host that makes requests: only the simulation's does.
    NoProvider,
    /// The connection under it failed, saying this.
    Net(NetError),
    /// A wait or an allocation stopped first.
    Stopped(Stopped),
    /// The answer is not HTTP/1.1 as the core reads it, which said this.
    Protocol(String),
    /// The answer's body passed its [`Wire::body_most`], `most` bytes.
    TooBig { most: usize },
    /// Its [`Wire::reads_most`], `reads` receives, did not bring the answer's end.
    Unending { reads: usize },
    /// A [`Http::handler`] left fewer bytes in the caller's [`Bytes`] than it was handed:
    /// `had` before it ran, `left` after. It may add to them, never take away.
    Shrank { had: usize, left: usize },
}

/// A request that failed: the call, the URL and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct HttpError {
    pub op: HttpOp,
    pub url: Url,
    pub kind: HttpKind,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::requests {:?} of {}: ", self.op, self.url)?;
        match &self.kind {
            HttpKind::NotAllowed => f.write_str("this Http does not reach its origin (it was narrowed)"),
            HttpKind::NoOrigin => f.write_str("it names no host to ask"),
            HttpKind::NotHttp => f.write_str("its scheme is neither http nor https"),
            HttpKind::NoTls => f.write_str(
                "https needs a sans-IO TLS core that takes a Roots, which bstd does not have yet \
                 (blaze-http's is inside its Client): Http::over speaks plain http only",
            ),
            HttpKind::NoProvider => f.write_str("this world has no host that makes requests"),
            HttpKind::Net(under) => write!(f, "{under}"),
            HttpKind::Stopped(stopped) => write!(f, "{stopped}"),
            HttpKind::Protocol(said) => write!(f, "{said}"),
            HttpKind::TooBig { most } => {
                write!(f, "its answer's body is more than the {most} bytes its Http was made to read")
            }
            HttpKind::Unending { reads } => {
                write!(f, "{reads} receives did not bring its answer's end (rule bounded-progress)")
            }
            HttpKind::Shrank { had, left } => write!(
                f,
                "its handler left {left} bytes in the caller's bytes, which held {had} before it \
                 ran: a handler adds to them and never takes away"
            ),
        }
    }
}

impl core::error::Error for HttpError {}

impl From<Stopped> for HttpKind {
    fn from(stopped: Stopped) -> HttpKind {
        HttpKind::Stopped(stopped)
    }
}

impl From<OutOfMemory> for HttpKind {
    fn from(full: OutOfMemory) -> HttpKind {
        HttpKind::Stopped(Stopped::from(full))
    }
}

/// What [`Http::handler`] takes: a request in, its answer's body added to the bytes and its
/// status out. It may wait under the `Cx` it is given.
pub type Handler = dyn Fn(&Cx, &Request<'_>, &mut Bytes<'_>) -> Result<u16, Stopped> + Send + Sync;

/// A route of the simulated host: what a request of `url` is answered.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's description of a world
pub struct SimRoute {
    pub url: Url,
    pub status: u16,
    pub body: Vec<u8>,
}

/// The simulated host's requests ([`crate::Io::http`]).
#[derive(Debug)]
pub struct SimHttp {
    /// What it answers; a URL with no route is answered 404 with no body.
    pub routes: Vec<SimRoute>,
    /// Every request it was asked, in order.
    // alloc: the simulated host's record, which lives as long as the world.
    pub asked: bmem::Vec<(Method, Url), Global>,
}

impl Default for SimHttp {
    fn default() -> SimHttp {
        SimHttp { routes: Vec::new(), asked: bmem::Vec::new_in(Global) }
    }
}

/// Who answers.
#[derive(Clone)]
enum Provider {
    Over {
        net: Net,
        /// Kept for the idle and transfer timeouts of blaze-http's `Client` core, once
        /// `over` drives it; a request's bound today is its `Cx`'s deadline.
        #[expect(dead_code, reason = "held for the Client core's timeouts; see the module's doc")]
        clock: Clock,
        /// Kept for TLS; trusted for nothing until then.
        #[expect(dead_code, reason = "held for TLS, which over refuses today; see the module's doc")]
        roots: Roots,
        wire: Wire,
    },
    Host(Io),
    Handler(Arc<Handler>),
}

/// Making requests: from [`Http::over`], [`crate::Io::http`] or [`Http::handler`].
#[derive(Clone)]
pub struct Http {
    provider: Provider,
    allowed: Allowed<Origin>,
    plant: Plant,
}

impl Http {
    /// Requests made by bstd itself, over connections from `net`, talking as `wire` says:
    /// plain `http://` only (see the module's doc for what it refuses and why). It makes one
    /// connection a request and never retries, redirects or pools.
    pub fn over(net: &Net, clock: &Clock, roots: &Roots, wire: Wire) -> Http {
        let plant = net.io().plant();
        // clone: authority held, copied: refcounts.
        let provider = Provider::Over { net: net.clone(), clock: clock.clone(), roots: roots.clone(), wire };
        Http { provider, allowed: Allowed::Any, plant }
    }

    /// Requests answered by `handler`: a fake for a test, a cache, a proxy.
    /// [`Stopped::OutOfMemory`] when the global heap has no room for the handler.
    pub fn handler(
        handler: impl Fn(&Cx, &Request<'_>, &mut Bytes<'_>) -> Result<u16, Stopped> + Send + Sync + 'static,
    ) -> Result<Http, Stopped> {
        // alloc: once per capability, its handler.
        let handler: Arc<Handler> = heap::shared(handler)?;
        Ok(Http { provider: Provider::Handler(handler), allowed: Allowed::Any, plant: Plant::None })
    }

    /// Requests the host makes for the program.
    pub(crate) fn host(io: Io) -> Http {
        let plant = io.plant();
        Http { provider: Provider::Host(io), allowed: Allowed::Any, plant }
    }

    /// Only requests to `origin`, if this reaches it already.
    pub fn only(&self, origin: &Origin) -> Http {
        if self.plant == Plant::HttpOnlyIgnored {
            // clone: the planted bug hands back what it was given.
            return self.clone();
        }
        // clone: authority held, narrowed: refcounts.
        Http { provider: self.provider.clone(), allowed: self.allowed.one(origin, self.plant), plant: self.plant }
    }

    /// `request` sent: its answer is read with [`Exchange::finish`].
    pub fn begin<'a>(&self, cx: &Cx, request: &Request<'a>) -> Result<Exchange<'a>, HttpError> {
        // clone: an error's name of its URL, refcounts.
        let refuse = |kind| HttpError { op: HttpOp::Begin, url: request.url.clone(), kind };
        let Some(origin) = request.url.origin() else { return Err(refuse(HttpKind::NoOrigin)) };
        if !self.allowed.allows(origin) {
            return Err(refuse(HttpKind::NotAllowed));
        }
        match origin.scheme().as_str() {
            "http" | "https" => {}
            _ => return Err(refuse(HttpKind::NotHttp)),
        }
        let (net, wire) = match &self.provider {
            // clone: the handler, a refcount.
            Provider::Handler(handler) => return Ok(Exchange { request: *request, state: State::Handler(Arc::clone(handler)) }),
            // clone: the world, a refcount.
            Provider::Host(io) => return Ok(Exchange { request: *request, state: State::Host(io.clone()) }),
            Provider::Over { net, wire, .. } => (net, *wire),
        };
        if origin.scheme().as_str() == "https" {
            return Err(refuse(HttpKind::NoTls));
        }
        let sent = sent_bytes(origin, request, wire).map_err(|full| refuse(HttpKind::from(full)))?;
        // clone: the address connected to, a refcount.
        let conn = match net.connect(cx, &Addr::Tcp(origin.at().clone())) {
            Ok(conn) => conn,
            Err(error) => return Err(refuse(HttpKind::Net(error))),
        };
        match conn.send_all(cx, &sent) {
            Ok(()) => Ok(Exchange { request: *request, state: State::Wire(conn, wire) }),
            Err(error) => {
                // The send's failure is the one told; the hangup's would say less.
                let _ = conn.finish(cx);
                Err(refuse(HttpKind::Net(error)))
            }
        }
    }

    /// `request` asked and its answer's body added to `into`: [`Http::begin`], then
    /// [`Exchange::finish`].
    pub fn request(&self, cx: &Cx, request: &Request<'_>, into: &mut Bytes<'_>) -> Result<Response, HttpError> {
        match self.begin(cx, request) {
            Ok(exchange) => exchange.finish(cx, into),
            Err(error) => Err(error),
        }
    }
}

impl fmt::Debug for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provider = match &self.provider {
            Provider::Over { .. } => "over a Net",
            Provider::Host(_) => "the host's",
            Provider::Handler(_) => "a handler",
        };
        write!(f, "Http({provider}, {:?})", self.allowed)
    }
}

/// A request in flight, finished with [`Exchange::finish`]. Linear (see the module's doc): a
/// panic that drops one hangs up its connection without waiting ([`Conn`]'s abandonment).
#[blaze::linear]
#[must_use = "a request in flight is finished with finish(cx, into)"]
pub struct Exchange<'a> {
    request: Request<'a>,
    state: State,
}

/// Where a request's answer comes from.
enum State {
    Handler(Arc<Handler>),
    Host(Io),
    /// Sent on this connection, which the answer is read from, within these bounds.
    Wire(Conn, Wire),
}

impl Exchange<'_> {
    /// The answer: its body added to `into` and its status. On an error `into` is as it was.
    /// A connection it was sent on is closed either way. The one way an `Exchange` ends.
    #[blaze::consumes]
    pub fn finish(self, cx: &Cx, into: &mut Bytes<'_>) -> Result<Response, HttpError> {
        let Exchange { request, state } = self;
        let start = into.len();
        let answered = match state {
            State::Handler(handler) => handler(cx, &request, into).map_err(HttpKind::Stopped),
            State::Host(io) => hosted(&io, cx, &request, into),
            State::Wire(conn, wire) => {
                let answered = read_answer(cx, &conn, &request, wire, into);
                // The answer, or why there is none, is the one told; the hangup's failure
                // would say less.
                let _ = conn.finish(cx);
                answered
            }
        };
        match answered {
            Ok(status) => match into.len().checked_sub(start) {
                Some(body) => Ok(Response { status, body }),
                None => {
                    // clone: an error's name of its URL, refcounts.
                    let kind = HttpKind::Shrank { had: start, left: into.len() };
                    Err(HttpError { op: HttpOp::Finish, url: request.url.clone(), kind })
                }
            },
            Err(kind) => {
                into.truncate(start);
                // clone: an error's name of its URL, refcounts.
                Err(HttpError { op: HttpOp::Finish, url: request.url.clone(), kind })
            }
        }
    }
}

impl fmt::Debug for Exchange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Exchange({:?} {})", self.request.method, self.request.url)
    }
}

/// The simulated host's answer to `request`: its route's, or 404.
fn hosted(io: &Io, cx: &Cx, request: &Request<'_>, into: &mut Bytes<'_>) -> Result<u16, HttpKind> {
    cx.stopped_by(io.now())?;
    let answered = io.script(|host| {
        // clone: the simulated host's record, refcounts.
        host.http.asked.push((request.method, request.url.clone()))?;
        match host.http.routes.iter().find(|route| &route.url == request.url) {
            Some(route) => into.extend(&route.body).map(|()| route.status),
            None => Ok(404),
        }
    });
    match answered {
        Some(answered) => Ok(answered?),
        None => Err(HttpKind::NoProvider),
    }
}

/// The bytes a request is sent as: its head as blaze-http's h1 writes it, then its body.
/// They are reserved whole before the core writes the head, so the core's own growth of its
/// `Vec` (which cannot fail by its signature) never allocates: [`HEAD_MOST_FIXED`] bounds what
/// it adds around the request's own text.
fn sent_bytes(origin: &Origin, request: &Request<'_>, wire: Wire) -> Result<Vec<u8>, OutOfMemory> {
    // The core's input: blaze-http's h1 takes the `Host` and the headers as std `String`s.
    let host = match origin.scheme().default_port() == Some(origin.at().port()) {
        // alloc: once per request, its `Host`.
        true => heap::string(origin.at().host())?,
        // alloc: once per request, its `Host`.
        false => origin.at().joined()?,
    };
    // alloc: once per request, its headers as the core takes them.
    let mut headers = bmem::Vec::with_capacity_in(request.headers.len(), Global)?;
    for header in request.headers {
        headers.push((heap::string(header.name())?, heap::string(header.value())?))?;
    }
    let headers = headers.into_std();
    // A sum past `usize` saturates, and the reservation of it is refused.
    let target = request.url.target();
    let own = [target.len(), host.len(), wire.user_agent.len(), request.body.len(), HEAD_MOST_FIXED];
    let each = headers.iter().map(|(name, value)| name.len().saturating_add(value.len()).saturating_add(HEADER_MOST_FIXED));
    let room = own.into_iter().chain(each).fold(0usize, usize::saturating_add);
    // alloc: once per request, its head and body as sent, freed once they are.
    let mut sent = bmem::Vec::with_capacity_in(room, Global)?.into_std();
    let method = request.method.of_core();
    h1::write_request(&mut sent, &method, target, &host, wire.user_agent, false, &headers, request.body.len(), None);
    let mut sent = bmem::Vec::from_std(sent);
    sent.extend_from_slice(request.body)?;
    Ok(sent.into_std())
}

/// The most bytes blaze-http's h1 writes around a request's target, host and user agent:
/// `PATCH ` and ` HTTP/1.1\r\n` (17), `host: ` and its line end (8), `user-agent: ` and its
/// (14), `content-length: ` with twenty digits and its (38), and the blank line (2), rounded up.
const HEAD_MOST_FIXED: usize = 128;

/// The most bytes it writes around one header's name and value: `: ` and the line end.
const HEADER_MOST_FIXED: usize = 4;

/// What a growth on the global heap that found no room says, for `asked` more bytes.
fn global_full(asked: usize) -> HttpKind {
    HttpKind::Stopped(Stopped::OutOfMemory { arena: Global.name(), asked })
}

/// The first `took` bytes of `unread` dropped, the rest moved to its start; a `took` past its
/// end is the core's error.
fn consume(unread: &mut bmem::Vec<u8, Global>, took: usize) -> Result<(), HttpKind> {
    let Some(left) = unread.len().checked_sub(took) else {
        // alloc: an error's message, which `HttpKind::Protocol` owns as a std `String`.
        return Err(said("blaze-http's h1 said it took more bytes than it was given"));
    };
    unread.as_mut_slice().copy_within(took.., 0);
    unread.truncate(left);
    Ok(())
}

/// [`HttpKind::Protocol`] saying `text`, or [`HttpKind::Stopped`] if the global heap has no
/// room for it.
fn said(text: &str) -> HttpKind {
    match heap::string(text) {
        Ok(said) => HttpKind::Protocol(said),
        Err(full) => HttpKind::from(full),
    }
}

/// The driver of [`Http::over`]: receives from `conn` fed to blaze-http's h1 core until the
/// answer to `request` is whole, its body added to `into`; its status. Bounded by `cx`'s
/// deadline, [`Wire::reads_most`] receives and [`Wire::body_most`] bytes of body.
fn read_answer(
    cx: &Cx,
    conn: &Conn,
    request: &Request<'_>,
    wire: Wire,
    into: &mut Bytes<'_>,
) -> Result<u16, HttpKind> {
    let cx = cx.span("bstd::requests: an answer's bytes");
    let start = into.len();
    // alloc: the core's input and output: blaze-http's h1 parses a head from, and decodes a
    // body into, a `Vec`. Freed as this returns.
    let (mut unread, mut decoded) = (bmem::Vec::new_in(Global), Vec::new());
    let mut received = cx.bytes();
    let mut head: Option<h1::Head> = None;
    for _ in 0..wire.reads_most {
        if head.is_none() {
            match h1::parse_head(&unread, request.method == Method::Head).map_err(HttpKind::Protocol)? {
                Some(parsed) => {
                    consume(&mut unread, parsed.len)?;
                    // An informational answer (1xx) is not the answer: the next head is.
                    if (100..200).contains(&parsed.status) {
                        continue;
                    }
                    head = Some(parsed);
                }
                None => {}
            }
        }
        if let Some(head) = &mut head {
            // A body decodes to no more bytes than it came in, so with this room the core's
            // own growth of `decoded` never allocates.
            decoded.clear();
            decoded.try_reserve(unread.len()).map_err(|_| global_full(unread.len()))?;
            let (took, ended) = head.body.decode(&unread, &mut decoded).map_err(HttpKind::Protocol)?;
            consume(&mut unread, took)?;
            into.extend(&decoded)?;
            decoded.clear();
            if into.len().saturating_sub(start) > wire.body_most {
                return Err(HttpKind::TooBig { most: wire.body_most });
            }
            if ended {
                return Ok(head.status);
            }
        }
        received.clear();
        let got = conn.recv(&cx, RECV, &mut received).map_err(HttpKind::Net)?;
        if got.is_zero() {
            // The peer closed: that ends a body framed by the close, and nothing else.
            return match &head {
                Some(head) => head.body.closed().map(|()| head.status).map_err(HttpKind::Protocol),
                None => Err(said("the connection closed before an answer's head was whole")),
            };
        }
        unread.extend_from_slice(received.as_slice())?;
    }
    Err(HttpKind::Unending { reads: wire.reads_most })
}
