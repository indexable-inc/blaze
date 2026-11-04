//! [`HyperHttp`]: the [`Http`] blaze asks servers through, over hyper 1 and rustls with ring
//! (the `net` feature).
//!
//! Its connections are its own: HTTP/1 on `hyper::client::conn::http1`, TLS from `tokio-rustls`
//! with the machine's roots, kept between requests in a [`Pool`] keyed by [`Origin`] (scheme,
//! host, port). No hyper-util: it pulls in `tracing`, which blaze no longer builds
//! (docs/v2/plan/observe/no-tracing.md, 6.1). No proxy is asked through, as none was before.

use std::{
    io::{self, Read},
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
    time::{Duration, Instant},
};

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode, Uri,
    body::{Bytes, Incoming},
    client::conn::http1::SendRequest,
    header::{self, HeaderValue},
};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};

use blaze_fetch::{redact, scrub};

use super::{Bounds, Http, Pending, Refused, Response, Sink, Transport};

/// How many redirects one request follows.
const REDIRECTS: usize = 10;
/// How many requests [`HyperHttp::get_async`] has in flight at once, each on a connection of
/// its own (HTTP/1), and so the most idle connections the [`Pool`] keeps.
const IN_FLIGHT: usize = 64;
/// How long an idle connection is kept for another request: hyper-util's default, which this
/// client had before it had its own pool.
const IDLE_FOR: Duration = Duration::from_secs(90);

/// The half of a connection a request is sent on; its driver is a task of the runtime.
type Sender = SendRequest<Full<Bytes>>;

/// [`Http`] over hyper and rustls, with the machine's certificates, on a runtime of its own
/// that any number of threads ask through at once.
pub struct HyperHttp {
    runtime: tokio::runtime::Runtime,
    client: Requests,
    in_flight: Arc<tokio::sync::Semaphore>,
}

/// What one request needs, cheap to clone into a task of the runtime.
#[derive(Clone)]
struct Requests {
    /// Connections between requests, shared by every clone.
    pool: Arc<parking_lot::Mutex<Pool<Sender>>>,
    /// rustls on ring with the machine's roots and no ALPN, as hyper-rustls's HTTP/1 connector
    /// had it.
    tls: tokio_rustls::TlsConnector,
    user_agent: Arc<str>,
    /// How long a connection or a transfer may go without a byte, cargo's `http.timeout`.
    timeout: Duration,
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
        let user_agent = "blaze (https://github.com/indexable-inc/blaze)".into();
        Ok(HyperHttp {
            runtime,
            client: Requests {
                // alloc: the process's one pool, made once with the client.
                pool: Arc::new(parking_lot::Mutex::new(Pool::new(IN_FLIGHT, IDLE_FOR))),
                tls: tls()?,
                user_agent,
                timeout,
            },
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
        self.runtime.block_on(self.client.fetch(
            method,
            url,
            headers,
            Bytes::from(body),
            Bounds::NONE,
            None,
        ))
    }

    /// [`Http::get`] of `url` held to `bounds`: a body past `bounds.most` stops being read, and
    /// with `bounds.https_only` a plain-HTTP address or redirect is refused ([`Refused`]).
    pub fn get_bounded(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        bounds: Bounds,
    ) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Bytes::new(), bounds, None))
    }
}

/// The TLS every HTTPS connection makes: rustls on ring at its safe default versions, checking
/// servers against the machine's roots (as hyper-rustls's `with_native_roots` read them: one
/// that does not parse is skipped, none at all is an error), with no client certificate and no
/// ALPN (an HTTP/1-only connector offered none).
fn tls() -> anyhow::Result<tokio_rustls::TlsConnector> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (added, skipped) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        anyhow::bail!(
            "the machine's certificates cannot be read: no root certificate to check servers with \
             ({skipped} did not parse; errors loading them: {:?})",
            found.errors
        );
    }
    // alloc: the client's TLS setup, made once with it.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls on ring has no safe default TLS versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // alloc: the client's TLS setup, made once with it.
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// Where a connection goes, the key of the [`Pool`]: a connection to one origin is never used
/// for another.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    https: bool,
    /// As the socket and TLS take it: an IPv6 address without its brackets.
    host: String,
    port: u16,
}

impl Origin {
    /// The origin `uri` (said as `said`) is asked of: `http` or `https` only, its port the
    /// scheme's when it names none.
    fn of(uri: &Uri, said: &str) -> anyhow::Result<Origin> {
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => anyhow::bail!(
                "{said}: its scheme {other:?} is neither http nor https, the only ones blaze-registry's HTTP client speaks"
            ),
        };
        let host = uri.host().with_context(|| format!("{said} names no host"))?;
        let host = host.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(host);
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        // alloc: one per request, the pool's key for its connection.
        Ok(Origin { https, host: host.to_owned(), port })
    }
}

/// The idle connections between requests, most recently idle last: a synchronous core that
/// owns no socket and reads no clock (`now` is given), so its rules are tested without IO.
/// It holds at most `most` connections, each for at most `keep`; a connection its driver has
/// ended, or a request failed on, is dropped as it is next looked at, through `open`.
struct Pool<S> {
    idle: Vec<Idle<S>>,
    most: usize,
    keep: Duration,
}

struct Idle<S> {
    origin: Origin,
    sender: S,
    since: Instant,
}

impl<S> Pool<S> {
    fn new(most: usize, keep: Duration) -> Pool<S> {
        // alloc: the client's idle list, made once and never grown past `most`.
        Pool { idle: Vec::with_capacity(most), most, keep }
    }

    /// The connection to `origin` idle the shortest, if `open` says it can take a request at
    /// `now`; every connection past `keep` or no longer open is dropped on the way.
    fn take(&mut self, origin: &Origin, now: Instant, open: impl Fn(&S) -> bool) -> Option<S> {
        let keep = self.keep;
        self.idle.retain(|it| now.saturating_duration_since(it.since) < keep && open(&it.sender));
        let at = self.idle.iter().rposition(|it| it.origin == *origin)?;
        Some(self.idle.remove(at).sender)
    }

    /// Keeps `sender`, idle from `now`; when the pool is full the one idle longest goes.
    fn put(&mut self, origin: Origin, sender: S, now: Instant) {
        if self.most == 0 {
            return;
        }
        if self.idle.len() >= self.most {
            self.idle.remove(0);
        }
        self.idle.push(Idle { origin, sender, since: now });
    }
}

impl Requests {
    /// `request` sent to `origin` (said as `said`), on an idle connection if the pool has an
    /// open one, else a new one. As hyper-util's client did, a request a reused connection
    /// closed under before it was written is sent once more, on a new connection; any other
    /// failure is the request's.
    async fn request(
        &self,
        request: Request<Full<Bytes>>,
        origin: &Origin,
        said: &str,
    ) -> anyhow::Result<(hyper::Response<Incoming>, Sender)> {
        let idle = self.pool.lock().take(origin, Instant::now(), Sender::is_ready);
        let reused = idle.is_some();
        let mut sender = match idle {
            Some(sender) => sender,
            None => self.connect(origin, said).await?,
        };
        let mut failed = match sender.try_send_request(request).await {
            Ok(answer) => return Ok((answer, sender)),
            Err(failed) => failed,
        };
        let unsent = failed.take_message().filter(|_| reused);
        let Some(request) = unsent else { return Err(asking(said, failed.into_error())) };
        let mut sender = self.connect(origin, said).await?;
        match sender.try_send_request(request).await {
            Ok(answer) => Ok((answer, sender)),
            Err(failed) => Err(asking(said, failed.into_error())),
        }
    }

    /// A new connection to `origin` for `said`: the socket, TLS for `https`, then HTTP/1 on
    /// it, each step's failure a [`Transport`] naming the host, the port and the step.
    async fn connect(&self, origin: &Origin, said: &str) -> anyhow::Result<Sender> {
        let Origin { https, ref host, port } = *origin;
        let failed = |step: &str, e: &dyn std::fmt::Display| {
            Transport(scrub(&format!("{step} {host}:{port} for {said}: {e}")))
        };
        let socket = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| failed("connecting to", &e))?;
        if !https {
            return handshake(Io(socket), &failed).await;
        }
        // alloc: one per connection, the name its certificate is checked against.
        let name = ServerName::try_from(host.clone()).map_err(|e| {
            anyhow::anyhow!("{said}: `{host}` is no name a TLS certificate can be checked against: {e}")
        })?;
        let tls = self.tls.connect(name, socket).await.map_err(|e| failed("TLS with", &e))?;
        handshake(Io(tls), &failed).await
    }

    /// Gives `sender`, whose answer's body has been read to its end or dropped, back to the
    /// pool for `origin` once it can take another request; a connection that closes instead
    /// (the body was cut short, or the server said `Connection: close`), or is not ready
    /// within the timeout, is dropped.
    async fn release(&self, origin: Origin, mut sender: Sender) {
        if !sender.is_ready() {
            let ready = tokio::time::timeout(self.timeout, sender.ready()).await;
            if !matches!(ready, Ok(Ok(()))) {
                return;
            }
        }
        self.pool.lock().put(origin, sender, Instant::now());
    }
}

/// HTTP/1 over `io`, its connection driven by a task of the runtime until the server closes it
/// or its sender is dropped; `failed` says what failed for whom.
async fn handshake<T>(
    io: Io<T>,
    failed: &impl Fn(&str, &dyn std::fmt::Display) -> Transport,
) -> anyhow::Result<Sender>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) =
        hyper::client::conn::http1::handshake(io).await.map_err(|e| failed("starting HTTP/1 with", &e))?;
    tokio::spawn(async move {
        // An error here cut short the request on the connection, whose answer or body says it;
        // one on an idle connection (the server closed it) is no one's, and the pool drops it.
        _ = connection.await;
    });
    Ok(sender)
}

/// A request that failed on its connection, said as `said`.
fn asking(said: &str, e: hyper::Error) -> anyhow::Error {
    Transport(scrub(&format!("asking {said}: {:#}", anyhow::Error::new(e)))).into()
}

/// A tokio socket as hyper reads and writes it (hyper-util's `TokioIo`, the one piece of it this
/// client needs).
struct Io<T>(T);

impl<T: AsyncRead + Unpin> hyper::rt::Read for Io<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // SAFETY: tokio's `ReadBuf` over the cursor's unfilled part only writes bytes into it and
        // never de-initializes one, which is all `as_mut` asks.
        let mut read = tokio::io::ReadBuf::uninit(unsafe { buf.as_mut() });
        match Pin::new(&mut self.0).poll_read(cx, &mut read) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        let n = read.filled().len();
        // SAFETY: tokio's `ReadBuf` counts as filled only bytes it initialized: the first `n`
        // of the cursor's unfilled part, which `advance` asks to be initialized.
        unsafe { buf.advance(n) };
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite + Unpin> hyper::rt::Write for Io<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }
}

/// The most of a streamed request's error body that is read ([`Http::stream`]): enough for
/// any server's message, and no more.
const ERROR_BODY_MOST: u64 = 64 << 10;

/// Where `address` redirecting to `to` (its `Location`, maybe relative) leads, held to
/// `bounds`: with `https_only`, a redirect off HTTPS is [`Refused::Downgraded`].
fn redirected(address: &str, to: &str, bounds: Bounds) -> anyhow::Result<String> {
    let next = url::Url::parse(address)?.join(to)?;
    if bounds.https_only && next.scheme() != "https" {
        return Err(Refused::Downgraded { from: redact(address), to: redact(next.as_str()) }.into());
    }
    Ok(next.to_string())
}

impl Requests {
    async fn fetch(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Bytes,
        bounds: Bounds,
        // A 200's body, a piece at a time as it comes, not decoded and not collected: the
        // answer's `body` is then empty. Any other answer's body is collected, for its error.
        mut sink: Option<Sink<'_>>,
    ) -> anyhow::Result<Response> {
        // `address` is asked; `said`, with no userinfo, query or fragment (a redirect's
        // `X-Amz-Signature`), is the only form of it any message holds.
        let mut address = url.to_owned();
        let mut said = redact(url);
        if bounds.https_only && url::Url::parse(url)?.scheme() != "https" {
            return Err(Refused::NotHttps { url: said }.into());
        }
        for _ in 0..=REDIRECTS {
            let uri: Uri = address.parse().with_context(|| format!("{said} is no address HTTP can ask"))?;
            let origin = Origin::of(&uri, &said)?;
            // Asked in origin form (path and query) with a `Host`, as hyper-util's client asked:
            // the connection is already to `origin`.
            let path = uri.path_and_query().map_or("/", |it| it.as_str());
            let host = uri.host().with_context(|| format!("{said} names no host"))?;
            let host = match uri.port_u16() {
                Some(port) if port != if origin.https { 443 } else { 80 } => {
                    // alloc: one per request, its `Host` header.
                    HeaderValue::from_str(&format!("{host}:{port}"))
                }
                _ => HeaderValue::from_str(host),
            }
            .with_context(|| format!("{said}: its host is no `Host` header"))?;
            let mut request = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(header::USER_AGENT, &*self.user_agent);
            // A streamed body is written out as it comes and hashed from there, so it is asked for
            // as it is kept: not encoded.
            if sink.is_none() {
                request = request.header(header::ACCEPT_ENCODING, "gzip");
            }
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let mut request = request.body(Full::new(body.clone()))?;
            request.headers_mut().entry(header::HOST).or_insert(host);
            // Connecting (the socket, TLS, HTTP/1) and the answer's head, as one wait.
            let (answer, sender) = tokio::time::timeout(self.timeout, self.request(request, &origin, &said))
                .await
                .map_err(|_| Transport(format!("no answer from {said} in {:?}", self.timeout)))??;
            let status = answer.status();
            if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
                let to = answer.headers().get(header::LOCATION).and_then(|it| it.to_str().ok());
                let to = to.with_context(|| format!("{said} redirects nowhere"))?;
                address = redirected(&address, to, bounds)?;
                said = redact(&address);
                drop(answer);
                self.release(origin, sender).await;
                continue;
            }
            let headers: Vec<(String, String)> = answer
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
                })
                .collect();
            let too_big = |length: Option<u64>, decoded: bool| Refused::TooBig {
                url: said.clone(),
                most: bounds.most,
                said: length,
                decoded,
            };
            let length = answer
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|it| it.to_str().ok()?.trim().parse::<u64>().ok());
            if let Some(length) = length.filter(|it| *it > bounds.most) {
                return Err(too_big(Some(length), false).into());
            }
            let mut body = answer.into_body();
            if let Some(sink) = sink.as_mut().filter(|_| status == StatusCode::OK) {
                let encoded = headers.iter().find(|(name, _)| name == "content-encoding");
                // A server's answer, not a lost connection: asking again gets the same, so it is
                // no `Transport` error and never tried again.
                if let Some((_, encoding)) = encoded.filter(|(_, it)| !it.eq_ignore_ascii_case("identity")) {
                    anyhow::bail!("{said} sent its body as `{encoding}`, which was not asked for, so none of it was kept");
                }
                let mut sent = 0u64;
                while let Some(chunk) = self.next(&mut body, &said).await? {
                    sent += chunk.len() as u64;
                    if sent > bounds.most {
                        return Err(too_big(None, false).into());
                    }
                    sink(&chunk).with_context(|| format!("keeping the body of {said}"))?;
                }
                drop(body);
                self.release(origin, sender).await;
                return Ok(Response { status: status.as_u16(), headers, body: Vec::new() });
            }
            // A streamed request's other answers are read for their error alone: their first
            // `ERROR_BODY_MOST` bytes, the rest left unread, so no server can fill the daemon's
            // memory with a 404 page. Any other request is held to its bounds.
            let streamed = sink.is_some();
            let most = if streamed { ERROR_BODY_MOST.min(bounds.most) } else { bounds.most };
            let mut bytes = Vec::new();
            while let Some(chunk) = self.next(&mut body, &said).await? {
                if bytes.len() as u64 + chunk.len() as u64 > most {
                    if streamed {
                        let room = (most as usize).saturating_sub(bytes.len()).min(chunk.len());
                        bytes.extend_from_slice(&chunk[..room]);
                        break;
                    }
                    return Err(too_big(None, false).into());
                }
                bytes.extend_from_slice(&chunk);
            }
            // Read to its end, or cut short at `most` (then the connection closes, not kept).
            drop(body);
            self.release(origin, sender).await;
            let mut response = Response { status: status.as_u16(), headers, body: bytes };
            if response.header("content-encoding").is_some_and(|it| it.eq_ignore_ascii_case("gzip"))
            {
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(&response.body[..])
                    .take(most.saturating_add(1))
                    .read_to_end(&mut decoded)
                    .map_err(|e| Transport(format!("{said} sent gzip that is not: {e}")))?;
                if decoded.len() as u64 > most {
                    if !streamed {
                        return Err(too_big(None, true).into());
                    }
                    decoded.truncate(most as usize);
                }
                response.body = decoded;
            }
            return Ok(response);
        }
        anyhow::bail!("{}: more than {REDIRECTS} redirects", redact(url))
    }

    /// The next piece of `body`; `said` is its address as messages say it ([`redact`]).
    async fn next(&self, body: &mut Incoming, said: &str) -> anyhow::Result<Option<Bytes>> {
        loop {
            let frame = tokio::time::timeout(self.timeout, body.frame()).await.map_err(|_| {
                Transport(format!("transfer from {said} stalled for {:?}", self.timeout))
            })?;
            let Some(frame) = frame else { return Ok(None) };
            let frame = frame.map_err(|e| Transport(format!("reading from {said}: {e}")))?;
            if let Ok(data) = frame.into_data() {
                return Ok(Some(data));
            }
        }
    }
}

impl Http for HyperHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        self.send(Method::GET, url, headers, Vec::new())
    }

    fn stream(&self, url: &str, headers: &[(&str, &str)], sink: Sink<'_>) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Bytes::new(), Bounds::NONE, Some(sink)))
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
            client.fetch(Method::GET, &url, &headers, Bytes::new(), Bounds::NONE, None).await
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
    use std::io::Write as _;

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
    /// error (worth another try) naming the host, the port and the step that failed.
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

    /// With `https_only` a redirect may move within HTTPS but never off it, to an absolute or
    /// a scheme-relative `http:` address; without it, any redirect is followed.
    #[test]
    fn an_https_only_redirect_to_plain_http_is_refused() {
        let https_only = Bounds { most: u64::MAX, https_only: true };
        let from = "https://github.com/o/r/releases/download/v1/a.tar.gz";
        assert_eq!(
            redirected(from, "https://objects.example/a?sig=1", https_only).unwrap(),
            "https://objects.example/a?sig=1"
        );
        assert_eq!(redirected(from, "/b", https_only).unwrap(), "https://github.com/b");
        for to in ["http://objects.example/a", "HTTP://objects.example/a"] {
            let got = redirected(from, to, https_only).unwrap_err();
            match got.downcast_ref::<Refused>() {
                Some(Refused::Downgraded { from: was, to: now }) => {
                    assert_eq!((was.as_str(), now.as_str()), (from, "http://objects.example/a"));
                }
                other => panic!("{to}: {other:?}"),
            }
        }
        assert_eq!(
            redirected(from, "http://objects.example/a", Bounds::NONE).unwrap(),
            "http://objects.example/a"
        );
    }
}
