//! [`HyperHttp`]: the [`Http`] blaze asks servers through (the `net` feature), on blaze-http:
//! its sans-IO HTTP/1.1 client (`blaze_http::Client`: connections kept between requests by
//! origin, redirects, gzip, TLS through rustls on ring with the machine's roots, every timeout
//! a deadline) driven by its tokio driver on a runtime of this client's own.
//!
//! The name is the first client's, kept so that no caller changes; nothing under it is hyper
//! any more (no hyper, hyper-util, hyper-rustls or tokio-rustls: docs/v2/plan/observe/no-tracing.md,
//! 6.1). No proxy is asked through, as none was before.

use std::{io::Read as _, sync::Arc, time::Duration};

use anyhow::Context as _;
use blaze_http::{
    Client, Config, Event, Kind, Request,
    tokio_driver::{Answer, Driver},
};
use http::Method;

use blaze_fetch::{redact, scrub};

use super::{Bounds, Http, Pending, Refused, Response, Sink, Transport};

/// How many requests [`HyperHttp::get_async`] has in flight at once, each on a connection of
/// its own (HTTP/1), and so the most idle connections the client keeps.
const IN_FLIGHT: usize = 64;

/// The most of a streamed request's error body that is read ([`Http::stream`]): enough for
/// any server's message, and no more.
const ERROR_BODY_MOST: u64 = 64 << 10;

/// [`Http`] over blaze-http, with the machine's certificates, on a runtime of its own that any
/// number of threads ask through at once.
pub struct HyperHttp {
    runtime: tokio::runtime::Runtime,
    client: Requests,
    in_flight: Arc<tokio::sync::Semaphore>,
}

/// What one request needs, cheap to clone into a task of the runtime.
#[derive(Clone)]
struct Requests {
    /// The driver of the process's one client: its connections, shared by every clone.
    driver: Driver,
}

impl HyperHttp {
    pub fn new(timeout: Duration) -> anyhow::Result<HyperHttp> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("blaze-registry http")
            .enable_all()
            .build()?;
        // No version: this crate is under every build of blaze, and the release blaze is is told
        // only to the engine (its build.rs), so that a release changes no crate below it.
        let user_agent = "blaze (https://github.com/indexable-inc/blaze)";
        let tls = blaze_http::roots::native()?;
        let config = Config { idle_most: IN_FLIGHT, ..Config::new(tls, user_agent, timeout) };
        let driver = Driver::spawn(runtime.handle(), Client::new(config));
        Ok(HyperHttp {
            runtime,
            client: Requests { driver },
            // alloc: the client's one semaphore, made once with it.
            in_flight: Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT)),
        })
    }

    /// One shared by the whole process, with cargo's default timeout of 30 seconds.
    pub fn shared() -> anyhow::Result<Arc<HyperHttp>> {
        #[expect(
            clippy::disallowed_types,
            reason = "the process's one HTTP client: its runtime and connection pool are shared by every fetch"
        )]
        static SHARED: std::sync::OnceLock<Result<Arc<HyperHttp>, String>> = std::sync::OnceLock::new();
        let shared = SHARED.get_or_init(|| {
            HyperHttp::new(Duration::from_secs(30)).map(Arc::new).map_err(|e| format!("{e:#}"))
        });
        shared.clone().map_err(anyhow::Error::msg)
    }

    /// `method` of `url` with `body`, after its redirects, as cargo's handle follows them.
    pub fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(method, url, headers, body, Bounds::NONE, None))
    }

    /// [`Http::get`] of `url` held to `bounds`: a body past `bounds.most` stops being read, and
    /// with `bounds.https_only` a plain-HTTP address or redirect is refused ([`Refused`]).
    pub fn get_bounded(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        bounds: Bounds,
    ) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Vec::new(), bounds, None))
    }
}

impl Requests {
    async fn fetch(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
        bounds: Bounds,
        // A 200's body, a piece at a time as it comes, not decoded and not collected: the
        // answer's `body` is then empty. Any other answer's body is collected, for its error.
        mut sink: Option<Sink<'_>>,
    ) -> anyhow::Result<Response> {
        let streamed = sink.is_some();
        let request = Request {
            method,
            // alloc: one per request, handed to the client.
            url: url.to_owned(),
            // alloc: one per request, its headers handed to the client.
            headers: headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect(),
            body,
            // A streamed body is written out as it comes and hashed from there, so it is asked
            // for as it is kept: not encoded. Any other is asked for as gzip, decoded by the
            // client and held to `most` once decoded.
            gzip: !streamed,
            most: bounds.most,
            https_only: bounds.https_only,
        };
        // Dropping the answer before its end cancels the request, closing its connection.
        let mut answer = self.driver.ask(request);
        // `said`, with no userinfo, query or fragment (a redirect's `X-Amz-Signature`), is the
        // only form of the address that answered any message holds.
        let (said, status, headers) = match next(&mut answer, url).await? {
            Event::Head { url, status, headers, .. } => (redact(&url), status, headers),
            Event::Failed { error, .. } => return Err(failed(error)),
            Event::Body { .. } | Event::Done { .. } => {
                anyhow::bail!("{}: blaze-http's client told of a body before its head", redact(url))
            }
        };
        if let Some(sink) = sink.as_mut().filter(|_| status == 200) {
            let encoded = headers.iter().find(|(name, _)| name == "content-encoding");
            // A server's answer, not a lost connection: asking again gets the same, so it is
            // no `Transport` error and never tried again.
            if let Some((_, encoding)) = encoded.filter(|(_, it)| !it.eq_ignore_ascii_case("identity")) {
                anyhow::bail!("{said} sent its body as `{encoding}`, which was not asked for, so none of it was kept");
            }
            // Held to `bounds.most` by the client, as sent (nothing is decoded).
            while let Some(piece) = body(&mut answer, &said).await? {
                sink(&piece).with_context(|| format!("keeping the body of {said}"))?;
            }
            return Ok(Response { status, headers, body: Vec::new() });
        }
        // A streamed request's other answers are read for their error alone: their first
        // `ERROR_BODY_MOST` bytes, the rest left unread (the answer dropped, its connection
        // closed), so no server can fill the daemon's memory with a 404 page. Any other request
        // is held to its bounds by the client.
        let most = if streamed { ERROR_BODY_MOST.min(bounds.most) } else { bounds.most };
        let room = usize::try_from(most).unwrap_or(usize::MAX);
        // alloc: one per answer, its body for the caller.
        let mut bytes = Vec::new();
        while let Some(piece) = body(&mut answer, &said).await? {
            let left = room.saturating_sub(bytes.len());
            if piece.len() > left {
                if !streamed {
                    // The client holds the body to `most` itself; past it is its bug, said here.
                    let refused = Refused::TooBig { url: said.clone(), most, said: None, decoded: true };
                    return Err(refused.into());
                }
                bytes.extend_from_slice(&piece[..left]);
                break;
            }
            bytes.extend_from_slice(&piece);
        }
        drop(answer);
        let mut response = Response { status, headers, body: bytes };
        // gzip a streamed request did not ask for, on an error's body: decoded for its message,
        // as far as `most`. Any other request's gzip the client decoded.
        if streamed && response.header("content-encoding").is_some_and(|it| it.eq_ignore_ascii_case("gzip")) {
            // alloc: one per gzip error body, at most `most` bytes and one.
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(&response.body[..])
                .take(most.saturating_add(1))
                .read_to_end(&mut decoded)
                .map_err(|e| Transport(format!("{said} sent gzip that is not: {e}")))?;
            decoded.truncate(room);
            response.body = decoded;
        }
        Ok(response)
    }
}

/// `answer`'s next event; `url` is the address asked, said only redacted.
async fn next(answer: &mut Answer, url: &str) -> anyhow::Result<Event> {
    answer.next().await.with_context(|| {
        format!(
            "asking {}: blaze-http's driver ended before the answer did (its runtime shut down, or its loop panicked)",
            redact(url)
        )
    })
}

/// The next piece of `answer`'s body, `None` at its end; `said` is the address that answered.
async fn body(answer: &mut Answer, said: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match next(answer, said).await? {
        Event::Body { bytes, .. } => Ok(Some(bytes)),
        Event::Done { .. } => Ok(None),
        Event::Failed { error, .. } => Err(failed(error)),
        Event::Head { .. } => anyhow::bail!("{said}: blaze-http's client told of a second head"),
    }
}

/// `error`, as blaze-registry's callers read it: a refusal is [`Refused`], a lost connection or
/// a timeout is [`Transport`] (worth another try), anything else names the address. Every
/// address is said redacted: `error.url` may hold a credential or a signature.
fn failed(error: blaze_http::Error) -> anyhow::Error {
    let said = redact(&error.url);
    match error.kind {
        Kind::NotHttps => Refused::NotHttps { url: said }.into(),
        Kind::Downgraded { to } => Refused::Downgraded { from: said, to: redact(&to) }.into(),
        Kind::TooBig { most, said: length, decoded } => Refused::TooBig { url: said, most, said: length, decoded }.into(),
        Kind::NoAnswer { after, .. } => Transport(format!("no answer from {said} in {after:?}")).into(),
        Kind::Stalled { after, .. } => Transport(format!("transfer from {said} stalled for {after:?}")).into(),
        Kind::Io { step, host, port, why } => Transport(scrub(&format!("{step} {host}:{port} for {said}: {why}"))).into(),
        Kind::Gzip(why) => Transport(format!("{said} sent gzip that is not: {why}")).into(),
        kind @ (Kind::Url(_) | Kind::Header(_) | Kind::Redirects { .. } | Kind::RedirectsNowhere { .. } | Kind::InUse) => {
            anyhow::anyhow!("{said}: {kind}")
        }
    }
}

impl Http for HyperHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        self.send(Method::GET, url, headers, Vec::new())
    }

    fn stream(&self, url: &str, headers: &[(&str, &str)], sink: Sink<'_>) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Vec::new(), Bounds::NONE, Some(sink)))
    }

    fn get_async(&self, url: &str, headers: &[(&str, &str)]) -> Pending<anyhow::Result<Response>> {
        let (client, in_flight) = (self.client.clone(), self.in_flight.clone());
        let url = url.to_owned();
        let headers: Vec<(String, String)> =
            headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        let task = self.runtime.spawn(async move {
            let _turn = in_flight.acquire_owned().await?;
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
            client.fetch(Method::GET, &url, &headers, Vec::new(), Bounds::NONE, None).await
        });
        Box::pin(async move { task.await.context("a request's task ended")? })
    }

    fn sleep(&self, duration: Duration) -> Pending<()> {
        let task = self.runtime.spawn(tokio::time::sleep(duration));
        Box::pin(async move {
            _ = task.await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// Serves each of `answers` to one connection, in order, on a local port: its address.
    fn serve(answers: Vec<Vec<u8>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/asset", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut asked = [0u8; 4096];
                _ = stream.read(&mut asked);
                _ = stream.write_all(&answer);
            }
        });
        address
    }

    /// A bounded request stops at its limit whether the server says the length, doesn't, or
    /// sends gzip that decodes past it, and with `https_only` never asks a plain-HTTP address.
    #[test]
    fn a_bounded_get_stops_at_its_limit_and_refuses_plain_http() {
        let big = vec![b'x'; 64];
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&big).unwrap();
        let gz = gz.finish().unwrap();
        let head = |extra: &str| format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{extra}\r\n");
        let said = [head("Content-Length: 64\r\n").into_bytes(), big.clone()].concat();
        let unsaid = [head("").into_bytes(), big.clone()].concat();
        let zipped = format!("Content-Encoding: gzip\r\nContent-Length: {}\r\n", gz.len());
        let zipped = [head(&zipped).into_bytes(), gz.clone()].concat();
        let small = [head("Content-Length: 3\r\n").into_bytes(), b"abc".to_vec()].concat();
        let url = serve(vec![said, unsaid, zipped, small]);
        assert!(gz.len() < 40, "the gzip is {} bytes", gz.len());
        let http = HyperHttp::new(Duration::from_secs(10)).unwrap();
        let bounds = Bounds { most: 40, https_only: false };
        let refused = |got: anyhow::Result<Response>| match got.unwrap_err().downcast::<Refused>() {
            Ok(Refused::TooBig { said, decoded, .. }) => (said, decoded),
            other => panic!("{other:?}"),
        };
        assert_eq!(refused(http.get_bounded(&url, &[], bounds)), (Some(64), false));
        assert_eq!(refused(http.get_bounded(&url, &[], bounds)), (None, false));
        assert_eq!(refused(http.get_bounded(&url, &[], bounds)), (None, true));
        assert_eq!(http.get_bounded(&url, &[], bounds).unwrap().body, b"abc");
        let https_only = Bounds { most: 40, https_only: true };
        let got = http.get_bounded(&url, &[], https_only).unwrap_err();
        assert!(matches!(got.downcast_ref::<Refused>(), Some(Refused::NotHttps { .. })), "{got:#}");
    }

    /// A transfer that stalls after a redirect to a signed address says no part of either
    /// address's query: no `?`, no `@` (a credential in it) and no signature in its message.
    #[test]
    fn a_stalled_transfer_says_no_credential() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let to = format!("http://127.0.0.1:{port}/b?X-Amz-Credential=me@example&X-Amz-Signature=secret");
        let answers = [
            format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_owned(),
        ];
        std::thread::Builder::new()
            .name("a server that stalls".to_owned())
            .spawn(move || {
                let mut open = Vec::new();
                for answer in answers {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut asked = [0u8; 4096];
                    _ = stream.read(&mut asked);
                    _ = stream.write_all(answer.as_bytes());
                    open.push(stream);
                }
                // Holds the second transfer open, 97 bytes short, until the test ends.
                _ = listener.accept();
                drop(open);
            })
            .unwrap();
        let http = HyperHttp::new(Duration::from_millis(500)).unwrap();
        let url = format!("http://127.0.0.1:{port}/a?token=secret@x");
        let got = http.stream(&url, &[], &mut |_| Ok(())).unwrap_err();
        let said = format!("{got:#}");
        assert!(got.downcast_ref::<Transport>().is_some(), "{said}");
        assert!(said.contains("stalled"), "{said}");
        assert!(said.contains(&format!("http://127.0.0.1:{port}/b")), "{said}");
        assert!(!said.contains('?') && !said.contains('@') && !said.contains("secret"), "{said}");
    }

    /// Requests to one origin take turns on one kept-alive connection, so a server that counts
    /// its connections sees one for three requests; a port no one listens on is a [`Transport`]
    /// error (worth another try) naming the host, the port and the step that failed. The one
    /// test of the tokio driver against real sockets; the client's rules are blaze-http's
    /// tests, with no IO.
    #[test]
    fn requests_reuse_a_pooled_connection_and_a_refused_one_says_where() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let server = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let listener = server.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&accepted);
        // Answers every request on a connection with a two-byte 200 and keeps it open; it
        // accepts at most four connections, and stops after a minute either way.
        let serving = async move {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut asked = Vec::new();
                    let mut piece = [0u8; 1024];
                    loop {
                        let end = loop {
                            if let Some(at) = asked.windows(4).position(|it| it == b"\r\n\r\n") {
                                break at + 4;
                            }
                            match stream.read(&mut piece).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => asked.extend_from_slice(&piece[..n]),
                            }
                        };
                        asked.drain(..end);
                        if stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.is_err() {
                            return;
                        }
                    }
                });
            }
        };
        std::thread::Builder::new()
            .name("a keep-alive server".to_owned())
            .spawn(move || server.block_on(async move { _ = tokio::time::timeout(Duration::from_secs(60), serving).await }))
            .unwrap();

        let http = HyperHttp::new(Duration::from_secs(10)).unwrap();
        let url = format!("http://127.0.0.1:{port}/a");
        for _ in 0..3 {
            let got = http.get(&url, &[]).unwrap();
            assert_eq!((got.status, &got.body[..]), (200, &b"ok"[..]));
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "three requests should have shared one connection");

        let refusing = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = refusing.local_addr().unwrap().port();
        drop(refusing);
        let got = http.get(&format!("http://127.0.0.1:{closed}/a"), &[]).unwrap_err();
        let said = format!("{got:#}");
        assert!(got.downcast_ref::<Transport>().is_some(), "{said}");
        assert!(said.contains(&format!("connecting to 127.0.0.1:{closed} for http://127.0.0.1:{closed}/a")), "{said}");
    }

    /// A redirect off HTTPS (blaze-http's tests follow one there) is [`Refused::Downgraded`],
    /// naming both addresses without their queries.
    #[test]
    fn an_https_only_redirect_to_plain_http_is_refused() {
        let error = blaze_http::Error {
            url: "https://github.com/o/r/releases/download/v1/a.tar.gz?sig=1".to_owned(),
            kind: Kind::Downgraded { to: "http://objects.example/a?sig=2".to_owned() },
        };
        match failed(error).downcast_ref::<Refused>() {
            Some(Refused::Downgraded { from, to }) => assert_eq!(
                (from.as_str(), to.as_str()),
                ("https://github.com/o/r/releases/download/v1/a.tar.gz", "http://objects.example/a")
            ),
            other => panic!("{other:?}"),
        }
    }
}
