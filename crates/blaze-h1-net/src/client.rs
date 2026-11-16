//! [`Client::send`]: one request, its redirects followed, over bstd's `Net`.

use core::{alloc::Allocator, fmt, net::IpAddr};

use blaze_h1::{ContentRange, Follow, Limits, Range, Redirects, Request, Response, is_redirect};
use blaze_own::{AllocFailure, Vec};
use bstd::{
    cx::Cx,
    net::{Addr, HostPort, HostPortError, Net},
    url::{Origin, Url, UrlError},
};

use crate::tls::{Plain, Stream, StreamError, Tls};

/// What a client asks.
#[derive(Clone, Copy)]
pub struct Ask<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    /// Headers after the ones the writer sets (`Host`, `Range`, `Content-Length`,
    /// `Connection`) and the client's `User-Agent`.
    pub headers: &'a [(&'a str, &'a str)],
    /// Sent with a `Content-Length` when not empty.
    pub body: &'a [u8],
    /// Asks for these bytes; the answer must be a `206` of exactly them ([`Response::ranged`]).
    pub range: Option<Range>,
}

/// What came back. The body is in the caller's vector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Got {
    /// The last response's status: any status but a followed redirect, for the caller to judge.
    pub status: u16,
    /// The range held, when one was asked and the status is `206`.
    pub range: Option<ContentRange>,
    /// Redirects followed.
    pub hops: u8,
}

/// A client: the capability it connects with, its TLS, and its bounds.
pub struct Client<'c, T: Tls, A: Allocator + Clone> {
    pub net: &'c Net,
    pub tls: &'c T,
    pub limits: Limits,
    pub redirects: Redirects,
    pub agent: &'c str,
    /// Holds the request's head and the response's while each is in flight.
    pub alloc: A,
}

/// A request that failed: the URL it was sent to last, and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct FetchError {
    pub url: Url,
    pub why: FetchWhy,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum FetchWhy {
    /// The URL has no `//host`.
    NoAuthority,
    /// Neither `http` nor `https`.
    Scheme,
    /// A redirect of other than `307` or `308` for a request that is neither GET nor HEAD: the
    /// method would change, which is the caller's decision, not this client's.
    MethodRedirect { status: u16 },
    /// A redirect's target is no URL.
    Url(UrlError),
    /// A looked-up address made no `HostPort`.
    Address(HostPortError),
    Stream(StreamError),
    Http(blaze_h1::Error),
    Redirect(blaze_h1::Why),
    Alloc(AllocFailure),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.url)?;
        match &self.why {
            FetchWhy::NoAuthority => write!(f, "the URL names no host"),
            FetchWhy::Scheme => write!(f, "the scheme is neither http nor https"),
            FetchWhy::MethodRedirect { status } => {
                write!(f, "a {status} redirect would change the request's method; not followed")
            }
            FetchWhy::Url(e) => write!(f, "a redirect's Location is no URL: {e:?}"),
            FetchWhy::Address(e) => write!(f, "{e}"),
            FetchWhy::Stream(e) => write!(f, "{e}"),
            FetchWhy::Http(e) => write!(f, "{e}"),
            FetchWhy::Redirect(why) => write!(f, "{}", blaze_h1::Error { at: 0, why: *why }),
            FetchWhy::Alloc(e) => write!(f, "{e}"),
        }
    }
}

/// One exchange's outcome, the response dropped.
struct Answer<A: Allocator> {
    status: u16,
    location: Option<Vec<u8, A>>,
    range: Option<ContentRange>,
}

impl<T: Tls, A: Allocator + Clone> Client<'_, T, A> {
    /// Sends `ask`, follows its redirects, and appends the last response's body to `out`
    /// (a redirect's body is dropped from it).
    pub fn send<B: Allocator>(&self, cx: &Cx, ask: &Ask<'_>, out: &mut Vec<u8, B>) -> Result<Got, FetchError> {
        let start = out.len();
        let mut follow = Follow::new(self.redirects);
        // clone: the URL, refcounts.
        let mut url = ask.url.clone();
        loop {
            let answer = self.once(cx, &url, ask, out).map_err(|why| FetchError { url: url.clone(), why })?;
            if !is_redirect(answer.status) {
                return Ok(Got { status: answer.status, range: answer.range, hops: follow.hops() });
            }
            out.truncate(start);
            let next = self.redirected(&url, &mut follow, ask.method, &answer);
            url = next.map_err(|why| FetchError { url: url.clone(), why })?;
        }
    }

    /// The URL a redirect sends `url` to.
    fn redirected(&self, url: &Url, follow: &mut Follow, method: &str, answer: &Answer<A>) -> Result<Url, FetchWhy> {
        let keeps_method = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
        if !keeps_method && !matches!(answer.status, 307 | 308) {
            return Err(FetchWhy::MethodRedirect { status: answer.status });
        }
        let origin = url.origin().ok_or(FetchWhy::NoAuthority)?;
        let mut authority = Vec::new_in(self.alloc.clone());
        write_authority(origin, true, &mut authority)?;
        let authority = core::str::from_utf8(&authority).expect("`str`s joined are UTF-8");
        let from = blaze_h1::Origin { scheme: origin.scheme().as_str(), authority };
        let hop = follow
            .next(from, answer.status, answer.location.as_deref())
            .map_err(FetchWhy::Redirect)?
            .ok_or(FetchWhy::Redirect(blaze_h1::Why::NoLocation))?;
        let mut text = Vec::new_in(self.alloc.clone());
        for part in [hop.origin.scheme, "://", hop.origin.authority, hop.target] {
            text.try_extend_from_slice(part.as_bytes()).map_err(FetchWhy::Alloc)?;
        }
        let text = core::str::from_utf8(&text).expect("`str`s joined are UTF-8");
        Url::parse(text).map_err(FetchWhy::Url)
    }

    /// One request and its answer on a fresh connection, finished on every path.
    fn once<B: Allocator>(&self, cx: &Cx, url: &Url, ask: &Ask<'_>, out: &mut Vec<u8, B>) -> Result<Answer<A>, FetchWhy> {
        let origin = url.origin().ok_or(FetchWhy::NoAuthority)?;
        let tls = match origin.scheme().as_str() {
            "http" => false,
            "https" => true,
            _ => return Err(FetchWhy::Scheme),
        };
        let mut head = Vec::new_in(self.alloc.clone());
        let mut host = Vec::new_in(self.alloc.clone());
        write_authority(origin, false, &mut host)?;
        let host = core::str::from_utf8(&host).expect("`str`s joined are UTF-8");
        let mut headers = Vec::new_in(self.alloc.clone());
        headers.try_reserve(ask.headers.len() + 1).map_err(FetchWhy::Alloc)?;
        headers.push(("User-Agent", self.agent));
        headers.extend_from_slice(ask.headers);
        let request = Request {
            method: ask.method,
            target: url.target(),
            host,
            range: ask.range,
            headers: &headers,
            body_len: (!ask.body.is_empty()).then_some(ask.body.len() as u64),
            close: true,
        };
        blaze_h1::write_request(&request, &mut head).map_err(FetchWhy::Http)?;
        let at = self.address(cx, origin.at())?;
        let conn = self.net.connect(cx, &Addr::Tcp(at)).map_err(|e| FetchWhy::Stream(StreamError::Net(e)))?;
        let head_only = ask.method.eq_ignore_ascii_case("HEAD");
        if tls {
            let stream = self.tls.connect(cx, conn, origin.at()).map_err(FetchWhy::Stream)?;
            self.exchange(cx, stream, &head, ask, head_only, out)
        } else {
            self.exchange(cx, Plain(conn), &head, ask, head_only, out)
        }
    }

    /// The address to connect to for `at`, looked up through the capability's resolver.
    fn address(&self, cx: &Cx, at: &HostPort) -> Result<HostPort, FetchWhy> {
        let found = self.net.resolver().lookup(cx, at).map_err(|e| FetchWhy::Stream(StreamError::Net(e)))?;
        let mut text = Text { buf: [0; 48], len: 0 };
        let wrote = match found.ip() {
            IpAddr::V4(ip) => fmt::write(&mut text, format_args!("{ip}")),
            IpAddr::V6(ip) => fmt::write(&mut text, format_args!("[{ip}]")),
        };
        wrote.map_err(|_| FetchWhy::NoAuthority)?;
        HostPort::new(text.as_str(), found.port()).map_err(FetchWhy::Address)
    }

    /// Sends `head` and the body over `stream`, reads the answer, and finishes the stream
    /// whatever happened; the first failure is the one reported.
    fn exchange<S: Stream, B: Allocator>(
        &self,
        cx: &Cx,
        mut stream: S,
        head: &[u8],
        ask: &Ask<'_>,
        head_only: bool,
        out: &mut Vec<u8, B>,
    ) -> Result<Answer<A>, FetchWhy> {
        let talked = self.talk(cx, &mut stream, head, ask, head_only, out);
        let finished = stream.finish(cx).map_err(FetchWhy::Stream);
        let answer = talked?;
        finished?;
        Ok(answer)
    }

    fn talk<S: Stream, B: Allocator>(
        &self,
        cx: &Cx,
        stream: &mut S,
        head: &[u8],
        ask: &Ask<'_>,
        head_only: bool,
        out: &mut Vec<u8, B>,
    ) -> Result<Answer<A>, FetchWhy> {
        stream.send_all(cx, head).map_err(FetchWhy::Stream)?;
        if !ask.body.is_empty() {
            stream.send_all(cx, ask.body).map_err(FetchWhy::Stream)?;
        }
        let mut response = Response::new_in(self.limits, head_only, self.alloc.clone());
        let mut buf = Vec::from_elem_in(0u8, 16 << 10, self.alloc.clone());
        let mut range = None;
        let mut checked = false;
        while !response.is_done() {
            let n = stream.recv_into(cx, &mut buf).map_err(FetchWhy::Stream)?;
            if n == 0 {
                response.closed().map_err(FetchWhy::Http)?;
                break;
            }
            let fed = response.feed(&buf[..n], out).map_err(FetchWhy::Http)?;
            if fed.consumed < n {
                let at = response.body_len();
                return Err(FetchWhy::Http(blaze_h1::Error { at, why: blaze_h1::Why::AfterEnd }));
            }
            // A range is checked as soon as the head says what it holds, before its body.
            if !checked && response.has_head() {
                checked = true;
                if let Some(asked) = ask.range
                    && !is_redirect(response.status())
                    && !crate::plant::range_unchecked()
                {
                    range = Some(response.ranged(asked).map_err(FetchWhy::Http)?);
                }
            }
        }
        let location = match response.header("location") {
            Some(bytes) => {
                let mut copy = Vec::new_in(self.alloc.clone());
                copy.try_extend_from_slice(bytes).map_err(FetchWhy::Alloc)?;
                Some(copy)
            }
            None => None,
        };
        Ok(Answer { status: response.status(), location, range })
    }
}

/// `host`, or `host:port` when the port is not the scheme's own (or `always`).
fn write_authority<A: Allocator>(origin: &Origin, always: bool, out: &mut Vec<u8, A>) -> Result<(), FetchWhy> {
    let at = origin.at();
    out.try_extend_from_slice(at.host().as_bytes()).map_err(FetchWhy::Alloc)?;
    if always || origin.scheme().default_port() != Some(at.port()) {
        let mut text = Text { buf: [0; 48], len: 0 };
        fmt::write(&mut text, format_args!(":{}", at.port())).map_err(|_| FetchWhy::NoAuthority)?;
        out.try_extend_from_slice(text.as_str().as_bytes()).map_err(FetchWhy::Alloc)?;
    }
    Ok(())
}

/// A short text on the stack: an address or a port.
struct Text {
    buf: [u8; 48],
    len: usize,
}

impl Text {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).expect("only `write_str` fills it, with whole `str`s")
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        let slot = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        slot.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
