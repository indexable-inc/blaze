//! [`HyperHttp`]: the [`Http`] blaze asks servers through, over hyper 1 and rustls with ring
//! (the `net` feature): TCP with HTTP/2 where the server's ALPN picks it, else HTTP/1.1, and
//! HTTP/3 over QUIC ([`super::quic`]) for a host [`choose`] picks it for (`net.http3`,
//! docs/networking.md).

use std::{
    io::Read,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::{
    HeaderMap, Method, Request, StatusCode, Version,
    body::{Bytes, Incoming},
    header,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tracing::Instrument as _;

use blaze_fetch::{redact, scrub};

use super::{
    Bounds, Http, Pending, Refused, Response, Sink, Transport,
    adverts::{self, Adverts},
    choice::{self, Http3Refused, Protocol, choose},
    dns::Dns,
    quic::{H3, Stream, Target},
    settings::Net,
};

/// How many redirects one request follows.
const REDIRECTS: usize = 10;
/// How many requests [`HyperHttp::get_async`] has in flight at once: each on a connection of
/// its own over HTTP/1.1, many on one over HTTP/2 or HTTP/3.
const IN_FLIGHT: usize = 64;

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
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    user_agent: Arc<str>,
    /// How long a connection or a transfer may go without a byte, cargo's `http.timeout`.
    timeout: Duration,
    net: Arc<Network>,
}

/// What chooses and carries HTTP/3: the settings, the adverts hosts made, DNS, and the QUIC
/// side, made at its first use.
struct Network {
    settings: Net,
    adverts: parking_lot::Mutex<Adverts>,
    /// Held while the adverts' file is written, so the last write is of the latest adverts.
    writing: parking_lot::Mutex<()>,
    /// One DNS HTTPS lookup per host at a time: the requests that arrive together wait for
    /// the first one's answer instead of asking again.
    dns_turns: blaze_hash::SharedMap<String, Arc<tokio::sync::Mutex<()>>>,
    dns: Dns,
    h3: tokio::sync::OnceCell<Result<H3, String>>,
    timeout: Duration,
}

/// The seconds since the epoch now: the clock the drivers hand [`Adverts`].
fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |it| it.as_secs())
}

impl Network {
    fn new(settings: Net, timeout: Duration) -> Network {
        let adverts = match &settings.adverts {
            Some(file) => {
                let (adverts, warning) = adverts::load(file, now());
                if let Some(warning) = warning {
                    eprintln!("warning: {warning}");
                }
                adverts
            }
            None => Adverts::new(),
        };
        Network {
            settings,
            adverts: parking_lot::Mutex::new(adverts),
            writing: parking_lot::Mutex::new(()),
            dns_turns: blaze_hash::SharedMap::default(),
            dns: Dns::new(),
            h3: tokio::sync::OnceCell::new(),
            timeout,
        }
    }

    /// The protocol of a request to `url` (`said` as messages say it): [`choose`] of the
    /// host's setting, its advert (asking its DNS HTTPS record first when that is not known
    /// and could change the choice), and the proxy the environment names for it.
    async fn protocol(&self, url: &url::Url, said: &str) -> anyhow::Result<Protocol> {
        let Some(host) = url.host_str() else { return Ok(Protocol::Tcp) };
        let setting = self.settings.setting_for(host);
        if setting == choice::Http3::Off {
            return Ok(Protocol::Tcp);
        }
        let https = url.scheme() == "https";
        let proxy = self.settings.proxies.for_host(host);
        let named = matches!(url.host(), Some(url::Host::Domain(_)));
        if https && proxy.is_none() && named && !self.adverts.lock().dns_known(host, now()) {
            let turn = {
                let turns = self.dns_turns.pin();
                turns.get_or_insert_with(host.to_owned(), || Arc::new(tokio::sync::Mutex::new(()))).clone() // alloc: once per host
            };
            let _turn = turn.lock().instrument(tracing::info_span!(target: "blaze", "dns https record turn", host)).await;
            if !self.adverts.lock().dns_known(host, now()) {
                let answer = self.dns.https_record(host).await;
                self.adverts.lock().learn_dns(host, answer, now());
                self.persist();
            }
        }
        let advert = self.adverts.lock().advert(host, now());
        choose(setting, host, said, https, advert, proxy)
            .map_err(|refusal| Http3Refused { refusal, config: self.settings.config.clone() }.into())
    }

    /// Keeps what an `Alt-Svc` header from `host` said, unless HTTP/3 is off for it.
    fn learned_alt_svc(&self, host: &str, headers: &HeaderMap) {
        let Some(said) = headers.get(header::ALT_SVC).and_then(|it| it.to_str().ok()) else { return };
        if self.settings.setting_for(host) == choice::Http3::Off {
            return;
        }
        if self.adverts.lock().learn_alt_svc(host, choice::alt_svc(said), now()) {
            self.persist();
        }
    }

    /// Writes the adverts to their file, if they are kept in one; a failure is a warning
    /// naming the file: this run goes on with them in memory.
    fn persist(&self) {
        let Some(file) = &self.settings.adverts else { return };
        let _writing = self.writing.lock();
        let bytes = {
            let mut adverts = self.adverts.lock();
            adverts.forget_expired(now());
            adverts.to_bytes()
        };
        if let Err(error) = adverts::save(file, &bytes) {
            eprintln!("warning: keeping the adverts of HTTP/3 in {}: {error}", file.display());
        }
    }

    /// The QUIC side, made at the first request over HTTP/3.
    async fn h3(&self) -> anyhow::Result<&H3> {
        self.h3
            .get_or_init(|| async { H3::new(self.timeout, self.settings.config.clone()).map_err(|e| format!("{e:#}")) })
            .await
            .as_ref()
            .map_err(|e| anyhow::anyhow!("making blaze's HTTP/3 client: {e}"))
    }
}

impl HyperHttp {
    /// A client with the user's `[net]` settings (`~/.blaze/config.toml`) and the process's
    /// proxies.
    pub fn new(timeout: Duration) -> anyhow::Result<HyperHttp> {
        HyperHttp::with_net(timeout, Net::from_user_config()?)
    }

    /// A client with `net` for its settings.
    pub fn with_net(timeout: Duration, net: Net) -> anyhow::Result<HyperHttp> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("blaze-registry http")
            .enable_all()
            .build()?;
        let roots = HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("the machine's certificates cannot be read")?;
        // ALPN offers h2 and http/1.1; the server picks.
        let connector = roots.https_or_http().enable_http1().enable_http2().build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        // No version: this crate is under every build of blaze, and the release blaze is is told
        // only to the engine (its build.rs), so that a release changes no crate below it.
        let user_agent = "blaze (https://github.com/indexable-inc/blaze)".into();
        Ok(HyperHttp {
            runtime,
            client: Requests { client, user_agent, timeout, net: Arc::new(Network::new(net, timeout)) },
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

/// An answer's body as it comes: over TCP (hyper), or on an HTTP/3 stream.
enum Body<'a> {
    Tcp(Incoming),
    H3 { h3: &'a H3, stream: Stream, target: Target<'a> },
}

/// What one request of a chain of redirects ended in.
enum Hop {
    /// A redirect to its `Location`, as given.
    Redirect(String),
    Done(Response),
}

/// One request of [`Requests::fetch`]'s chain: what is asked, and how.
struct Ask<'a> {
    method: &'a Method,
    address: &'a str,
    said: &'a str,
    host: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: &'a Bytes,
    bounds: Bounds,
    protocol: Protocol,
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
            let parsed = url::Url::parse(&address).with_context(|| format!("{said} is not a URL"))?;
            let host = parsed.host_str().unwrap_or_default().to_owned();
            let protocol = self.net.protocol(&parsed, &said).await?;
            let (protocol_name, source) = match protocol {
                // Which of HTTP/2 and HTTP/1.1 the server picked is known at its answer.
                Protocol::Tcp => ("tcp", "none"),
                Protocol::H3 { source, .. } => ("h3", source.name()),
            };
            let span = tracing::info_span!(
                target: "blaze",
                "http request",
                method = %method,
                host = %host,
                protocol = protocol_name,
                source,
                status = tracing::field::Empty,
            );
            let ask = Ask { method: &method, address: &address, said: &said, host: &host, headers, body: &body, bounds, protocol };
            match self.hop(&ask, &mut sink).instrument(span).await? {
                Hop::Redirect(to) => {
                    address = redirected(&address, &to, bounds)?;
                    said = redact(&address);
                }
                Hop::Done(response) => return Ok(response),
            }
        }
        anyhow::bail!("{}: more than {REDIRECTS} redirects", redact(url))
    }

    /// Sends one request of a chain and reads its answer, or the redirect it is.
    async fn hop(&self, ask: &Ask<'_>, sink: &mut Option<Sink<'_>>) -> anyhow::Result<Hop> {
        let said = ask.said;
        let mut request = Request::builder()
            .method(ask.method.clone())
            .uri(ask.address)
            .header(header::USER_AGENT, &*self.user_agent);
        // A streamed body is written out as it comes and hashed from there, so it is asked for
        // as it is kept: not encoded.
        if sink.is_none() {
            request = request.header(header::ACCEPT_ENCODING, "gzip");
        }
        for (name, value) in ask.headers {
            request = request.header(*name, *value);
        }
        let (status, answer_headers, mut body) = match ask.protocol {
            Protocol::Tcp => {
                let request = request.body(Full::new(ask.body.clone()))?;
                let answer = tokio::time::timeout(self.timeout, self.client.request(request))
                    .await
                    .map_err(|_| Transport(format!("no answer from {said} in {:?}", self.timeout)))?
                    .map_err(|e| Transport(scrub(&format!("asking {said}: {e:#}"))))?;
                let version = match answer.version() {
                    Version::HTTP_2 => "h2",
                    Version::HTTP_11 => "http/1.1",
                    _ => "http/1.0",
                };
                tracing::Span::current().record("protocol", version);
                let (parts, incoming) = answer.into_parts();
                (parts.status, parts.headers, Body::Tcp(incoming))
            }
            Protocol::H3 { port, source } => {
                let h3 = self.net.h3().await?;
                let target = Target { host: ask.host, port, source, said };
                let request = request.body(())?;
                let (status, headers, stream) = h3.send(&target, request, ask.body.clone()).await?;
                (status, headers, Body::H3 { h3, stream, target })
            }
        };
        tracing::Span::current().record("status", status.as_u16());
        self.net.learned_alt_svc(ask.host, &answer_headers);
        if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
            let to = answer_headers.get(header::LOCATION).and_then(|it| it.to_str().ok());
            let to = to.with_context(|| format!("{said} redirects nowhere"))?;
            return Ok(Hop::Redirect(to.to_owned()));
        }
        let bounds = ask.bounds;
        let headers: Vec<(String, String)> = answer_headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
            .collect();
        let too_big = |length: Option<u64>, decoded: bool| Refused::TooBig {
            url: said.to_owned(),
            most: bounds.most,
            said: length,
            decoded,
        };
        let length = answer_headers
            .get(header::CONTENT_LENGTH)
            .and_then(|it| it.to_str().ok()?.trim().parse::<u64>().ok());
        if let Some(length) = length.filter(|it| *it > bounds.most) {
            return Err(too_big(Some(length), false).into());
        }
        if let Some(sink) = sink.as_mut().filter(|_| status == StatusCode::OK) {
            let encoded = headers.iter().find(|(name, _)| name == "content-encoding");
            // A server's answer, not a lost connection: asking again gets the same, so it is
            // no `Transport` error and never tried again.
            if let Some((_, encoding)) = encoded.filter(|(_, it)| !it.eq_ignore_ascii_case("identity")) {
                anyhow::bail!("{said} sent its body as `{encoding}`, which was not asked for, so none of it was kept");
            }
            let mut sent = 0u64;
            while let Some(chunk) = self.next(&mut body, said).await? {
                sent += chunk.len() as u64;
                if sent > bounds.most {
                    return Err(too_big(None, false).into());
                }
                sink(&chunk).with_context(|| format!("keeping the body of {said}"))?;
            }
            return Ok(Hop::Done(Response { status: status.as_u16(), headers, body: Vec::new() }));
        }
        // A streamed request's other answers are read for their error alone: their first
        // `ERROR_BODY_MOST` bytes, the rest left unread, so no server can fill the daemon's
        // memory with a 404 page. Any other request is held to its bounds.
        let streamed = sink.is_some();
        let most = if streamed { ERROR_BODY_MOST.min(bounds.most) } else { bounds.most };
        let mut bytes = Vec::new();
        while let Some(chunk) = self.next(&mut body, said).await? {
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
        let mut response = Response { status: status.as_u16(), headers, body: bytes };
        if response.header("content-encoding").is_some_and(|it| it.eq_ignore_ascii_case("gzip")) {
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
        Ok(Hop::Done(response))
    }

    /// The next piece of `body`; `said` is its address as messages say it ([`redact`]).
    async fn next(&self, body: &mut Body<'_>, said: &str) -> anyhow::Result<Option<Bytes>> {
        let body = match body {
            Body::Tcp(body) => body,
            Body::H3 { h3, stream, target } => return Ok(h3.next(target, stream).await?),
        };
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
        let http = HyperHttp::with_net(Duration::from_secs(10), Net::off()).unwrap();
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
        let http = HyperHttp::with_net(Duration::from_millis(500), Net::off()).unwrap();
        let url = format!("http://127.0.0.1:{port}/a?token=secret@x");
        let got = http.stream(&url, &[], &mut |_| Ok(())).unwrap_err();
        let said = format!("{got:#}");
        assert!(got.downcast_ref::<Transport>().is_some(), "{said}");
        assert!(said.contains("stalled"), "{said}");
        assert!(said.contains(&format!("http://127.0.0.1:{port}/b")), "{said}");
        assert!(!said.contains('?') && !said.contains('@') && !said.contains("secret"), "{said}");
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
