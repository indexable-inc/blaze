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

use std::{fmt, sync::Arc};

use blaze_http::h1;

use crate::{
    Plant, Stopped,
    clock::Clock,
    cx::Cx,
    io::Io,
    mem::Bytes,
    narrow::Allowed,
    net::{Addr, Conn, Net, NetError},
    roots::Roots,
    url::{Origin, Url},
};

/// The most bytes of an answer's body [`Http::over`] reads: past it the request fails
/// ([`HttpKind::TooBig`]).
pub const BODY_MOST: usize = 64 << 20;

/// The most receives [`Http::over`] makes for one answer before it stops waiting for its end
/// (rule bounded-progress).
pub const READS_MOST: usize = 1 << 16;

/// The most bytes one receive of [`Http::over`] asks for.
const RECV: usize = 16 << 10;

/// What [`Http::over`] calls itself.
const USER_AGENT: &str = "bstd";

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
    name: Arc<str>,
    value: Arc<str>,
}

/// What [`Header::new`] refused: the header's name and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct HeaderError {
    pub name: String,
    pub why: String,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "header {:?}: {}", self.name, self.why)
    }
}

impl std::error::Error for HeaderError {}

impl Header {
    pub fn new(name: &str, value: &str) -> Result<Header, HeaderError> {
        if let Some(why) = h1::refuse_header(name, value) {
            // alloc: an error's message.
            return Err(HeaderError { name: name.to_owned(), why });
        }
        // alloc: once per header, its name and value.
        Ok(Header { name: Arc::from(name), value: Arc::from(value) })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
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
    /// The answer's body passed [`BODY_MOST`] bytes.
    TooBig,
    /// [`READS_MOST`] receives did not bring the answer's end.
    Unending,
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
            HttpKind::TooBig => write!(f, "its answer's body is more than {BODY_MOST} bytes"),
            HttpKind::Unending => {
                write!(f, "{READS_MOST} receives did not bring its answer's end (rule bounded-progress)")
            }
        }
    }
}

impl std::error::Error for HttpError {}

impl From<Stopped> for HttpKind {
    fn from(stopped: Stopped) -> HttpKind {
        HttpKind::Stopped(stopped)
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
#[derive(Debug, Default)]
pub struct SimHttp {
    /// What it answers; a URL with no route is answered 404 with no body.
    pub routes: Vec<SimRoute>,
    /// Every request it was asked, in order.
    pub asked: Vec<(Method, Url)>,
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
    /// Requests made by bstd itself, over connections from `net`: plain `http://` only (see
    /// the module's doc for what it refuses and why).
    pub fn over(net: &Net, clock: &Clock, roots: &Roots) -> Http {
        let plant = net.io().plant();
        // clone: authority held, copied: refcounts.
        let provider = Provider::Over { net: net.clone(), clock: clock.clone(), roots: roots.clone() };
        Http { provider, allowed: Allowed::Any, plant }
    }

    /// Requests answered by `handler`: a fake for a test, a cache, a proxy.
    pub fn handler(
        handler: impl Fn(&Cx, &Request<'_>, &mut Bytes<'_>) -> Result<u16, Stopped> + Send + Sync + 'static,
    ) -> Http {
        // alloc: once per capability, its handler.
        Http { provider: Provider::Handler(Arc::new(handler)), allowed: Allowed::Any, plant: Plant::None }
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
        let net = match &self.provider {
            // clone: the handler, a refcount.
            Provider::Handler(handler) => return Ok(Exchange { request: *request, state: State::Handler(Arc::clone(handler)) }),
            // clone: the world, a refcount.
            Provider::Host(io) => return Ok(Exchange { request: *request, state: State::Host(io.clone()) }),
            Provider::Over { net, .. } => net,
        };
        if origin.scheme().as_str() == "https" {
            return Err(refuse(HttpKind::NoTls));
        }
        // The core's input: blaze-http's h1 writes a request's head into a `Vec`.
        let host = match origin.scheme().default_port() == Some(origin.at().port()) {
            // alloc: once per request, its `Host`.
            true => origin.at().host().to_owned(),
            // alloc: once per request, its `Host`.
            false => origin.at().to_string(),
        };
        // alloc: once per request, its headers as the core takes them.
        let headers: Vec<(String, String)> =
            request.headers.iter().map(|it| (it.name().to_owned(), it.value().to_owned())).collect();
        // alloc: once per request, its head and body as sent.
        let mut wire = Vec::new();
        let method = request.method.of_core();
        h1::write_request(&mut wire, &method, request.url.target(), &host, USER_AGENT, false, &headers, request.body.len(), None);
        wire.extend_from_slice(request.body);
        // clone: the address connected to, a refcount.
        let conn = match net.connect(cx, &Addr::Tcp(origin.at().clone())) {
            Ok(conn) => conn,
            Err(error) => return Err(refuse(HttpKind::Net(error))),
        };
        match conn.send(cx, &wire) {
            Ok(()) => Ok(Exchange { request: *request, state: State::Wire(conn) }),
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
    /// Sent on this connection, which the answer is read from.
    Wire(Conn),
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
            State::Wire(conn) => {
                let answered = read_answer(cx, &conn, &request, into);
                // The answer, or why there is none, is the one told; the hangup's failure
                // would say less.
                let _ = conn.finish(cx);
                answered
            }
        };
        match answered {
            Ok(status) => Ok(Response { status, body: into.len() - start }),
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
        host.http.asked.push((request.method, request.url.clone()));
        match host.http.routes.iter().find(|route| &route.url == request.url) {
            Some(route) => into.try_extend(&route.body).map(|()| route.status),
            None => Ok(404),
        }
    });
    match answered {
        Some(answered) => Ok(answered?),
        None => Err(HttpKind::NoProvider),
    }
}

/// The driver of [`Http::over`]: receives from `conn` fed to blaze-http's h1 core until the
/// answer to `request` is whole, its body added to `into`; its status. Bounded by `cx`'s
/// deadline, [`READS_MOST`] receives and [`BODY_MOST`] bytes of body.
fn read_answer(cx: &Cx, conn: &Conn, request: &Request<'_>, into: &mut Bytes<'_>) -> Result<u16, HttpKind> {
    let cx = cx.span("bstd::requests: an answer's bytes");
    let start = into.len();
    // alloc: the core's input and output: blaze-http's h1 parses a head from, and decodes a
    // body into, a `Vec`. Freed as this returns.
    let (mut unread, mut decoded): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    let mut received = cx.bytes();
    let mut head: Option<h1::Head> = None;
    for _ in 0..READS_MOST {
        if head.is_none() {
            match h1::parse_head(&unread, request.method == Method::Head).map_err(HttpKind::Protocol)? {
                Some(parsed) => {
                    unread.drain(..parsed.len);
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
            let (took, ended) = head.body.decode(&unread, &mut decoded).map_err(HttpKind::Protocol)?;
            unread.drain(..took);
            into.try_extend(&decoded)?;
            decoded.clear();
            if into.len() - start > BODY_MOST {
                return Err(HttpKind::TooBig);
            }
            if ended {
                return Ok(head.status);
            }
        }
        received.clear();
        let got = conn.recv(&cx, RECV, &mut received).map_err(HttpKind::Net)?;
        if got == 0 {
            // The peer closed: that ends a body framed by the close, and nothing else.
            return match &head {
                Some(head) => head.body.closed().map(|()| head.status).map_err(HttpKind::Protocol),
                // alloc: an error's message.
                None => Err(HttpKind::Protocol("the connection closed before an answer's head was whole".to_owned())),
            };
        }
        unread.extend_from_slice(received.as_slice());
    }
    Err(HttpKind::Unending)
}
