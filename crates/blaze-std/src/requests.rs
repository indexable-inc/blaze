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
//! **What `over` does and refuses.** It drives blaze-h1's core (the request writer and the
//! response parser, bytes in and bytes out) with a thin driver over a
//! [`Conn`]: one connection a request, closed when its answer is read; no pool and no gzip.
//! Everything else is checked by blaze-h1's own functions, none here: a [`Request::range`] is
//! sent as `Range` and its answer must be a `206` of exactly those bytes
//! ([`blaze_h1::Response::ranged`], checked as the head ends, before a body byte is taken); a
//! body is held to [`Wire::body_most`], or to the range's length when that is less, by the
//! parser's [`blaze_h1::Limits`] before a byte past it is taken; a byte after the answer's end
//! is refused ([`blaze_h1::Why::AfterEnd`]); a redirect is followed only as far as
//! [`Wire::redirects`] allows ([`blaze_h1::Follow`]; [`NO_REDIRECTS`] refuses every one), each
//! hop a [`Finished::Redirected`] so the begin/finish split holds across hops; the connect and
//! every receive wait at most [`Wire::wait_most`], a host that says nothing for that long being
//! [`HttpKind::Silent`] (rule bounded-progress). `https://` is refused
//! ([`HttpKind::NoTls`]): the only TLS in the tree is inside blaze-http's `Client`, over a
//! rustls configuration, and bstd has no sans-IO TLS core that takes a [`Roots`] yet. The
//! `Roots` handed in is kept for that day and trusted for nothing until then. A host that is a
//! name is refused by the `Net` ([`crate::net::NetKind::NoLookup`]) until blaze-core can look
//! one up.
//!
//! **Finishing.** A request in flight is an [`Exchange`], linear: [`Http::begin`] sends the
//! request and [`Exchange::finish`] reads its answer and closes its connection, so neither a
//! connection nor an answer's error can be dropped unseen. A followed redirect finishes as the
//! next hop's `Exchange`, already sent. [`Http::request`] is the two together, hop by hop.

use alloc::{borrow::ToOwned, string::{String, ToString}, sync::Arc, vec::Vec};
use core::{fmt, time::Duration};

use blaze_h1::{ContentRange, Follow, HeaderFault, Range, Redirects};

use crate::{
    Plant, Stopped,
    clock::Clock,
    cx::Cx,
    io::Io,
    mem::Bytes,
    narrow::Allowed,
    net::{Addr, Conn, Net, NetError, NetKind},
    roots::Roots,
    unfinished::Unfinished,
    url::{Origin, Url, UrlError},
};

/// The redirect policy that follows none: a redirect is [`HttpKind::Redirect`] with
/// [`blaze_h1::Why::RedirectsOver`].
pub const NO_REDIRECTS: Redirects = Redirects { most: 0, cross_scheme: false, cross_origin: false };

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
    /// The longest the connect, a send or one receive waits: a host silent that long is
    /// [`HttpKind::Silent`]. The request's `Cx` deadline bounds the whole as well.
    #[blaze::policy]
    pub wait_most: Duration,
    /// Which redirects are followed, and how many: [`NO_REDIRECTS`] refuses every one.
    #[blaze::policy]
    pub redirects: Redirects,
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
    /// Its name on the wire.
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// A request header, checked as it is made: a name that is a token, a value with no control
/// byte, and not one of the headers the writer sets itself (`Host`, `Range`,
/// `Content-Length`, `Transfer-Encoding`, `Connection`).
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

impl core::error::Error for HeaderError {}

impl Header {
    #[blaze::nonblocking]
    pub fn new(name: &str, value: &str) -> Result<Header, HeaderError> {
        if let Err(fault) = blaze_h1::check_header(name, value) {
            let why = match fault {
                HeaderFault::NotToken => "is no header name",
                HeaderFault::Owned => "is the client's to say: it frames the body, keeps the connection or is a field of the request",
                HeaderFault::ControlByte => "has a line break, a NUL or another control byte in its value",
            };
            // alloc: an error's message.
            return Err(HeaderError { name: name.to_owned(), why: why.to_owned() });
        }
        // alloc: once per header, its name and value.
        Ok(Header { name: Arc::from(name), value: Arc::from(value) })
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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
    /// Only these bytes: [`Http::over`] sends a `Range` and takes only a `206` of exactly them.
    /// A handler or the host are handed it and judge it themselves.
    pub range: Option<Range>,
}

impl<'a> Request<'a> {
    /// A `GET` of `url` with no headers.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(url: &'a Url) -> Request<'a> {
        Request { method: Method::Get, url, headers: &[], body: &[], range: None }
    }
}

/// An answer: its status, how many bytes of body were added to the caller's bytes, the range
/// they are (checked against the one asked) and the redirects followed to it. Its other
/// headers are not handed back yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Response {
    pub status: u16,
    pub body: usize,
    /// The `Content-Range` held, when a range was asked: the one asked for, or up to the
    /// resource's end when it ends sooner.
    pub range: Option<ContentRange>,
    pub hops: u8,
}

/// How an [`Exchange`] finished: answered, or redirected to the next hop, already sent.
#[must_use = "a redirect's next hop is finished in its turn"]
pub enum Finished<'a> {
    Answered(Response),
    Redirected(Exchange<'a>),
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
    /// The answer is not HTTP/1.1 as blaze-h1 reads it, which said why and at which byte.
    Protocol(blaze_h1::Error),
    /// The answer's body passed its [`Wire::body_most`], `most` bytes.
    TooBig { most: usize },
    /// Its [`Wire::reads_most`], `reads` receives, did not bring the answer's end.
    Unending { reads: usize },
    /// The host said nothing for its [`Wire::wait_most`] (a connect, a send or one receive).
    Silent { waited: Duration },
    /// A redirect not followed, by the policy ([`Wire::redirects`]) or for want of a target:
    /// blaze-h1's [`Follow`] said why.
    Redirect(blaze_h1::Why),
    /// A `status` redirect of a request that is neither GET nor HEAD, other than 307 or 308:
    /// following it would change the method, which is the caller's decision.
    MethodRedirect { status: u16 },
    /// A redirect's target is no URL.
    Location(UrlError),
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
            HttpKind::Silent { waited } => {
                write!(f, "its host said nothing for {waited:?}, its Wire::wait_most (rule bounded-progress)")
            }
            HttpKind::Redirect(why) => {
                write!(f, "a redirect not followed: {}", blaze_h1::Error { at: 0, why: *why })
            }
            HttpKind::MethodRedirect { status } => {
                write!(f, "a {status} redirect would change its method; not followed")
            }
            HttpKind::Location(why) => write!(f, "a redirect's Location is no URL: {why:?}"),
        }
    }
}

impl core::error::Error for HttpError {}

impl From<Stopped> for HttpKind {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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
        /// What [`Wire::wait_most`] is counted on.
        clock: Clock,
        /// Kept for TLS; trusted for nothing until then.
        #[expect(dead_code, reason = "held for TLS, which over refuses today; see the module's doc")]
        #[expect(blaze::unwired, reason = "unwired until `over` speaks TLS, which it refuses today")]
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
    #[blaze::pure]
    #[blaze::nonblocking]
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
    #[track_caller]
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
        // clone: the URL asked, a refcount.
        let url = request.url.clone();
        let line = match &self.provider {
            // clone: the handler, a refcount.
            Provider::Handler(handler) => return Ok(Exchange::made(*request, url, State::Handler(Arc::clone(handler)))),
            // clone: the world, a refcount.
            Provider::Host(io) => return Ok(Exchange::made(*request, url, State::Host(io.clone()))),
            // clone: authority held for the hops, refcounts.
            Provider::Over { net, clock, wire, .. } => {
                Line { net: net.clone(), clock: clock.clone(), wire: *wire, allowed: self.allowed.clone() }
            }
        };
        match send(cx, &line, request, &url) {
            Ok(conn) => {
                let follow = Follow::new(line.wire.redirects);
                Ok(Exchange::made(*request, url, State::Wire { conn, line, follow }))
            }
            Err(kind) => Err(refuse(kind)),
        }
    }

    /// `request` asked and its answer's body added to `into`: [`Http::begin`], then
    /// [`Exchange::finish`] of each hop its redirects allow (bounded by [`Wire::redirects`]).
    pub fn request(&self, cx: &Cx, request: &Request<'_>, into: &mut Bytes<'_>) -> Result<Response, HttpError> {
        let mut exchange = self.begin(cx, request)?;
        // Bounded: each pass is a hop `Follow` counted, and it refuses the one past its most.
        loop {
            match exchange.finish(cx, into)? {
                Finished::Answered(response) => return Ok(response),
                Finished::Redirected(next) => exchange = next,
            }
        }
    }
}

/// What sending again needs, kept by a request in flight for its redirects: the capability's
/// net, clock, bounds and reach.
struct Line {
    net: Net,
    clock: Clock,
    wire: Wire,
    allowed: Allowed<Origin>,
}

/// The wait `cx` allows one step of a request on `line`: [`Wire::wait_most`], never past `cx`'s.
fn step(cx: &Cx, line: &Line) -> Cx {
    cx.until(line.clock.after(line.wire.wait_most))
}

/// `error` of a step under `wait`, a child of `cx`: [`HttpKind::Silent`] when it is the step's
/// own deadline that passed, not `cx`'s.
fn waited(cx: &Cx, wait: &Cx, wire: Wire, error: NetError) -> HttpKind {
    let own = wait.deadline() != cx.deadline();
    match error.kind {
        NetKind::Stopped(Stopped::Deadline { .. }) if own => HttpKind::Silent { waited: wire.wait_most },
        _ => HttpKind::Net(error),
    }
}

/// `request` sent to `url` on a new connection from `line`: the connection its answer comes on.
fn send(cx: &Cx, line: &Line, request: &Request<'_>, url: &Url) -> Result<Conn, HttpKind> {
    let Some(origin) = url.origin() else { return Err(HttpKind::NoOrigin) };
    if !line.allowed.allows(origin) {
        return Err(HttpKind::NotAllowed);
    }
    match origin.scheme().as_str() {
        "http" => {}
        "https" => return Err(HttpKind::NoTls),
        _ => return Err(HttpKind::NotHttp),
    }
    let wire = line.wire;
    {
        // The core's input: blaze-h1 writes a request's head into a `Vec`.
        let host = match origin.scheme().default_port() == Some(origin.at().port()) {
            // alloc: once per request, its `Host`.
            true => origin.at().host().to_owned(),
            // alloc: once per request, its `Host`.
            false => origin.at().to_string(),
        };
        // alloc: once per request, its headers as the core takes them: the user agent, then
        // the request's.
        let mut headers: Vec<(&str, &str)> = Vec::with_capacity(1 + request.headers.len());
        headers.push(("user-agent", wire.user_agent));
        headers.extend(request.headers.iter().map(|it| (it.name(), it.value())));
        let body = request.body.len() as u64;
        let carries = matches!(request.method, Method::Post | Method::Put | Method::Patch);
        let head = blaze_h1::Request {
            method: request.method.as_str(),
            target: url.target(),
            host: &host,
            range: request.range,
            headers: &headers,
            body_len: (body > 0 || carries).then_some(body),
            close: true,
        };
        // alloc: once per request, its head and body as sent.
        let mut sent = Vec::new();
        blaze_h1::write_request(&head, &mut sent).map_err(HttpKind::Protocol)?;
        sent.extend_from_slice(request.body);
        let wait = step(cx, line);
        // clone: the address connected to, a refcount.
        let conn = match line.net.connect(&wait, &Addr::Tcp(origin.at().clone())) {
            Ok(conn) => conn,
            Err(error) => return Err(waited(cx, &wait, wire, error)),
        };
        let wait = step(cx, line);
        match conn.send_all(&wait, &sent) {
            Ok(()) => Ok(conn),
            Err(error) => {
                // The send's failure is the one told; the hangup's would say less.
                let _ = conn.finish(cx);
                Err(waited(cx, &wait, wire, error))
            }
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
    /// Where it was sent: the request's URL, or a redirect's.
    url: Url,
    state: State,
    unfinished: Unfinished,
}

/// Where a request's answer comes from.
enum State {
    Handler(Arc<Handler>),
    Host(Io),
    /// Sent on this connection, which the answer is read from, within the line's bounds; the
    /// redirects followed so far.
    Wire { conn: Conn, line: Line, follow: Follow },
}

impl<'a> Exchange<'a> {
    /// The request in flight, made where [`Http::begin`] was called.
    #[track_caller]
    fn made(request: Request<'a>, url: Url, state: State) -> Exchange<'a> {
        Exchange { request, url, state, unfinished: Unfinished::new("requests::Exchange", "finish(cx, into)") }
    }

    /// The answer, its body added to `into`, or the next hop of a redirect its policy follows,
    /// already sent. On an error, or a redirect, `into` is as it was. A connection it was sent
    /// on is closed either way. The one way an `Exchange` ends.
    #[blaze::consumes]
    pub fn finish(self, cx: &Cx, into: &mut Bytes<'_>) -> Result<Finished<'a>, HttpError> {
        let Exchange { request, url, state, unfinished } = self;
        unfinished.finished();
        let start = into.len();
        let answered = match state {
            State::Handler(handler) => handler(cx, &request, into).map_err(HttpKind::Stopped),
            State::Host(io) => hosted(&io, cx, &request, into),
            State::Wire { conn, line, mut follow } => {
                let answered = read_answer(cx, &conn, &request, &line, into);
                // The answer, or why there is none, is the one told; the hangup's failure
                // would say less.
                let _ = conn.finish(cx);
                let answer = match answered {
                    Ok(answer) => answer,
                    Err(kind) => {
                        into.truncate(start);
                        return Err(HttpError { op: HttpOp::Finish, url, kind });
                    }
                };
                if !blaze_h1::is_redirect(answer.status) {
                    let body = into.len() - start;
                    let response = Response { status: answer.status, body, range: answer.range, hops: follow.hops() };
                    return Ok(Finished::Answered(response));
                }
                into.truncate(start);
                let next = match hop(&url, &mut follow, request.method, answer.status, answer.location.as_deref()) {
                    Ok(next) => next,
                    Err(kind) => return Err(HttpError { op: HttpOp::Finish, url, kind }),
                };
                return match send(cx, &line, &request, &next) {
                    Ok(conn) => Ok(Finished::Redirected(Exchange::made(request, next, State::Wire { conn, line, follow }))),
                    Err(kind) => Err(HttpError { op: HttpOp::Begin, url: next, kind }),
                };
            }
        };
        match answered {
            Ok(status) => Ok(Finished::Answered(Response { status, body: into.len() - start, range: None, hops: 0 })),
            Err(kind) => {
                into.truncate(start);
                Err(HttpError { op: HttpOp::Finish, url, kind })
            }
        }
    }
}

impl fmt::Debug for Exchange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Exchange({:?} {})", self.request.method, self.url)
    }
}

/// Where a `status` redirect to `location` of a `method` request sent to `url` goes, if
/// `follow`'s policy takes it there (blaze-h1's [`Follow`] judges; nothing is parsed here).
fn hop(url: &Url, follow: &mut Follow, method: Method, status: u16, location: Option<&[u8]>) -> Result<Url, HttpKind> {
    if !matches!(method, Method::Get | Method::Head) && !matches!(status, 307 | 308) {
        return Err(HttpKind::MethodRedirect { status });
    }
    let Some(origin) = url.origin() else { return Err(HttpKind::NoOrigin) };
    // alloc: once per hop, its origin's `host:port` as the core compares it.
    let authority = origin.at().to_string();
    let from = blaze_h1::Origin { scheme: origin.scheme().as_str(), authority: &authority };
    let next = match follow.next(from, status, location) {
        Ok(Some(next)) => next,
        Ok(None) => return Err(HttpKind::Redirect(blaze_h1::Why::NoLocation)),
        Err(why) => return Err(HttpKind::Redirect(why)),
    };
    // alloc: once per hop, its URL's text.
    let mut text = String::new();
    for part in [next.origin.scheme, "://", next.origin.authority, next.target] {
        text.push_str(part);
    }
    Url::parse(&text).map_err(HttpKind::Location)
}

/// The planted bug of the driver's simulation (rule 53): the `Content-Range` check skipped,
/// set by a test for one closure on its own thread. Outside tests it is `false` and folds away.
#[cfg(test)]
pub(crate) mod plant {
    std::thread_local! {
        static RANGE_UNCHECKED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }

    /// Whether the test on this thread has removed the `Content-Range` check.
    pub(crate) fn range_unchecked() -> bool {
        RANGE_UNCHECKED.with(core::cell::Cell::get)
    }

    /// Puts the check back as it was when dropped.
    struct Back(bool);

    impl Drop for Back {
        fn drop(&mut self) {
            RANGE_UNCHECKED.with(|it| it.set(self.0));
        }
    }

    /// `body` run with the check removed (`planted`) or as it is, and the check put back after.
    pub(crate) fn with<R>(planted: bool, body: impl FnOnce() -> R) -> R {
        let _back = Back(RANGE_UNCHECKED.with(|it| it.replace(planted)));
        body()
    }
}

#[cfg(not(test))]
mod plant {
    #[inline(always)]
    pub(crate) const fn range_unchecked() -> bool {
        false
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
/// answer to `request` is whole and its connection closed, its body added to `into`. Bounded
/// by [`Wire::wait_most`] a receive, [`Wire::reads_most`] receives and [`Wire::body_most`]
/// bytes of body (or the range's length, when less). A range asked is checked as soon as the
/// head is whole, before any of the body is added to `into`.
fn read_answer(cx: &Cx, conn: &Conn, request: &Request<'_>, line: &Line, into: &mut Bytes<'_>) -> Result<Answer, HttpKind> {
    let cx = cx.span("bstd::requests: an answer's bytes");
    let wire = line.wire;
    // The body is held by the core itself, before a byte past it is written, to `body_most`
    // or the range asked, whichever is less; the head to 64 KiB and 100 lines.
    let most_body = match request.range {
        Some(Range { first, last: Some(last) }) if last >= first => (last - first + 1).min(wire.body_most as u64),
        Some(_) | None => wire.body_most as u64,
    };
    let limits = blaze_h1::Limits { most_head: 64 << 10, most_headers: 100, most_body, ..blaze_h1::Limits::SHIPPED };
    // alloc: the core's head and its body's next piece, `Vec`s freed as this returns.
    let mut answer = blaze_h1::Response::new_in(limits, request.method == Method::Head, alloc::alloc::Global);
    let mut decoded: Vec<u8> = Vec::new();
    let mut received = cx.bytes();
    let over = |error: blaze_h1::Error| match error.why {
        blaze_h1::Why::BodyOver { .. } => HttpKind::TooBig { most: most_body as usize },
        _ => HttpKind::Protocol(error),
    };
    let after_end = |answer: &blaze_h1::Response<_>| {
        HttpKind::Protocol(blaze_h1::Error { at: answer.body_len(), why: blaze_h1::Why::AfterEnd })
    };
    let mut range = None;
    let mut checked = false;
    for _ in 0..wire.reads_most {
        received.clear();
        let wait = step(&cx, line);
        let got = conn.recv(&wait, RECV, &mut received).map_err(|error| waited(&cx, &wait, wire, error))?;
        if answer.is_done() {
            // It asked for the close: anything but the close after the answer's end is a
            // byte the answer did not promise.
            if got > 0 {
                return Err(after_end(&answer));
            }
            break;
        }
        if got == 0 {
            // The peer closed: that ends a body framed by the close, and nothing else.
            answer.closed().map_err(HttpKind::Protocol)?;
            break;
        }
        let fed = answer.feed(received.as_slice(), &mut decoded);
        // A range is checked as soon as the head says what it holds, before its body is taken,
        // and before the body's bound is told: a head the bound refuses (a 404's longer body)
        // is named by what it answered.
        // blaze-h1 refuses a `Content-Length` past the bound as its head ends: the status and
        // headers are read by then, so the range is checked first.
        let head_over = matches!(&fed, Err(blaze_h1::Error { why: blaze_h1::Why::BodyOver { .. }, .. }));
        if !checked && (answer.has_head() || head_over) {
            checked = true;
            if let Some(asked) = request.range
                && !blaze_h1::is_redirect(answer.status())
                && !plant::range_unchecked()
            {
                range = Some(answer.ranged(asked).map_err(HttpKind::Protocol)?);
            }
        }
        let fed = fed.map_err(over)?;
        if fed.consumed < got {
            return Err(after_end(&answer));
        }
        into.try_extend(&decoded)?;
        decoded.clear();
    }
    if !answer.is_done() {
        return Err(HttpKind::Unending { reads: wire.reads_most });
    }
    // alloc: once per redirect, its `Location` as sent.
    let location = answer.header("location").map(<[u8]>::to_vec);
    Ok(Answer { status: answer.status(), range, location })
}

/// One answer read: its status, the range it holds when one was asked, and its `Location`.
struct Answer {
    status: u16,
    range: Option<ContentRange>,
    location: Option<Vec<u8>>,
}
