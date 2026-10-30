//! Asking a server for a file: the [`Http`] trait the index and the downloads go through, the
//! retries of cargo's `net.retry` (`src/util/network/retry.rs`), and [`HyperHttp`], over the
//! HTTP stack the rest of blaze uses (hyper 1 and rustls with ring).

use std::{
    fmt,
    future::Future,
    io::Read,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode, Uri,
    body::{Body, Bytes, Incoming},
    header,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::{
        legacy::{
            Client,
            connect::{
                HttpConnector,
                proxy::{SocksV4, SocksV5, Tunnel},
            },
        },
        proxy::matcher::{Intercept, Matcher},
    },
    rt::TokioExecutor,
};

/// An answer to a request, after its redirects, with its body decoded.
#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, it)| it.as_str())
    }
}

/// Work started at once and finished on whichever thread does it, awaited from any executor.
pub type Pending<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What asks servers for files. An error is one where no answer came, which is worth trying
/// again, as cargo tries a connection or a transfer that failed again.
pub trait Http: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response>;

    /// [`Http::get`], sent before this returns and answered when it is done, so that many
    /// requests are in flight at once. By default it is asked and answered on this thread.
    fn get_async(&self, url: &str, headers: &[(&str, &str)]) -> Pending<anyhow::Result<Response>> {
        Box::pin(std::future::ready(self.get(url, headers)))
    }

    /// A wait that holds no thread; by default it holds this one.
    fn sleep(&self, duration: Duration) -> Pending<()> {
        std::thread::sleep(duration);
        Box::pin(std::future::ready(()))
    }
}

/// An answer that is not what was asked for.
#[derive(Debug)]
pub struct HttpNotSuccessful {
    pub url: String,
    pub code: u16,
    pub retry_after: Option<String>,
    pub body: String,
}

impl HttpNotSuccessful {
    pub fn new(url: &str, response: &Response) -> HttpNotSuccessful {
        let mut body = String::from_utf8_lossy(&response.body).into_owned();
        body.truncate(body.floor_char_boundary(512));
        HttpNotSuccessful {
            url: url.to_owned(),
            code: response.status,
            retry_after: response.header("retry-after").map(str::to_owned),
            body,
        }
    }

    fn is_spurious(&self) -> bool {
        (500..600).contains(&self.code) || self.code == 429
    }
}

impl fmt::Display for HttpNotSuccessful {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to get successful HTTP response from `{}`, got {}", self.url, self.code)?;
        if !self.body.trim().is_empty() {
            write!(f, "\nbody:\n{}", self.body.trim())?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpNotSuccessful {}

/// How many times a failed request is tried again, cargo's `net.retry`, and whether to say so.
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    pub retries: u32,
    pub quiet: bool,
    /// A wait between tries in place of cargo's, as cargo's
    /// `__CARGO_TEST_FIXED_RETRY_SLEEP_MS`, for tests.
    pub fixed_sleep_ms: Option<u64>,
}

impl Default for Retry {
    fn default() -> Retry {
        Retry { retries: 3, quiet: false, fixed_sleep_ms: None }
    }
}

const MAX_RETRY_SLEEP_MS: u64 = 10 * 1000;
const INITIAL_RETRY_SLEEP_BASE_MS: u64 = 500;
const INITIAL_RETRY_JITTER_MS: u64 = 1000;

impl Retry {
    /// Runs `attempt` until it succeeds, fails in a way that does not change by trying again,
    /// or has been tried `retries` more times, sleeping between as cargo sleeps.
    pub fn run<T>(&self, mut attempt: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut tried = 0;
        loop {
            match attempt() {
                Err(e) => match self.again(&mut tried, &e) {
                    Some(wait) => std::thread::sleep(wait),
                    None => return Err(e),
                },
                it => return it,
            }
        }
    }

    /// [`Retry::run`] of an attempt that waits, sleeping with `http`, so that the thread runs
    /// other requests in between.
    pub async fn run_async<T, F: Future<Output = anyhow::Result<T>>>(
        &self,
        http: &dyn Http,
        mut attempt: impl FnMut() -> F,
    ) -> anyhow::Result<T> {
        let mut tried = 0;
        loop {
            match attempt().await {
                Err(e) => match self.again(&mut tried, &e) {
                    Some(wait) => http.sleep(wait).await,
                    None => return Err(e),
                },
                it => return it,
            }
        }
    }

    /// How long to wait before trying again after `e`, said as cargo says it; none when `e`
    /// is not worth another try or the tries are used up.
    fn again(&self, tried: &mut u32, e: &anyhow::Error) -> Option<Duration> {
        if *tried >= self.retries || !spurious(e) {
            return None;
        }
        let left = self.retries - *tried;
        if !self.quiet {
            let tries = if left == 1 { "try" } else { "tries" };
            eprintln!("warning: spurious network error ({left} {tries} remaining): {e:#}");
        }
        *tried += 1;
        let after = e.downcast_ref::<HttpNotSuccessful>().and_then(retry_after);
        let sleep = after.map_or_else(
            || self.fixed_sleep_ms.unwrap_or_else(|| sleep_ms(*tried)),
            |it| it.min(MAX_RETRY_SLEEP_MS),
        );
        Some(Duration::from_millis(sleep))
    }
}

/// A transfer that never answered, or an answer of a server error or of too many requests.
fn spurious(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<HttpNotSuccessful>() {
        Some(answer) => answer.is_spurious(),
        None => e.downcast_ref::<Transport>().is_some(),
    }
}

/// The wait before try `tried + 1`: half a second and up to a second more at random first,
/// then three seconds more each time, to ten.
fn sleep_ms(tried: u32) -> u64 {
    if tried == 1 {
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        INITIAL_RETRY_SLEEP_BASE_MS + u64::from(nanos.subsec_nanos()) % INITIAL_RETRY_JITTER_MS
    } else {
        (u64::from(tried - 1) * 3 * 1000 + INITIAL_RETRY_SLEEP_BASE_MS).min(MAX_RETRY_SLEEP_MS)
    }
}

/// A `Retry-After` of seconds, on the answers it applies to, 429 and 503.
fn retry_after(answer: &HttpNotSuccessful) -> Option<u64> {
    if !matches!(answer.code, 429 | 503) {
        return None;
    }
    let seconds: u32 = answer.retry_after.as_deref()?.trim().parse().ok()?;
    Some(u64::from(seconds) * 1000)
}

/// A request that got no answer: refused, reset, timed out, or cut short.
#[derive(Debug)]
pub struct Transport(pub String);

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Transport {}

type DirectClient<B> = Client<HttpsConnector<HttpConnector>, B>;
type TunnelClient<B> = Client<HttpsConnector<Tunnel<HttpsConnector<HttpConnector>>>, B>;
type SocksV5Client<B> = Client<HttpsConnector<SocksV5<HttpConnector>>, B>;
type SocksV4Client<B> = Client<HttpsConnector<SocksV4<HttpConnector>>, B>;

enum ProxyClient<B> {
    Tunnel(TunnelClient<B>),
    SocksV5(SocksV5Client<B>),
    SocksV4(SocksV4Client<B>),
}

/// An HTTP(S) client over hyper 1 and rustls that honors proxy environment variables
/// (`CARGO_HTTP_PROXY`, `HTTPS_PROXY`/`https_proxy`, `HTTP_PROXY`/`http_proxy`,
/// `ALL_PROXY`/`all_proxy`, `NO_PROXY`/`no_proxy`).
/// Cargo's override wins (an empty value disables proxies); otherwise lowercase names
/// take precedence. In Windows CGI, neither HTTP proxy alias is trusted. Unlike curl,
/// uppercase `HTTP_PROXY` is also accepted outside CGI (`REQUEST_METHOD` absent).
pub struct HttpClient<B> {
    inner: Arc<ClientInner<B>>,
}

struct ClientInner<B> {
    direct: DirectClient<B>,
    connect_timeout: Option<Duration>,
    matcher: Matcher,
    http_proxy: OnceLock<ProxyClient<B>>,
    https_proxy: OnceLock<ProxyClient<B>>,
}

impl<B> Clone for HttpClient<B> {
    fn clone(&self) -> Self {
        HttpClient { inner: Arc::clone(&self.inner) }
    }
}

impl<B> HttpClient<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    pub fn new(connect_timeout: Option<Duration>) -> anyhow::Result<HttpClient<B>> {
        HttpClient::with_matcher(connect_timeout, matcher_from_env()?)
    }

    pub fn with_matcher(
        connect_timeout: Option<Duration>,
        matcher: Matcher,
    ) -> anyhow::Result<HttpClient<B>> {
        let direct = Client::builder(TokioExecutor::new()).build(https_connector(connect_timeout)?);
        Ok(HttpClient {
            inner: Arc::new(ClientInner {
                direct,
                connect_timeout,
                matcher,
                http_proxy: OnceLock::new(),
                https_proxy: OnceLock::new(),
            }),
        })
    }

    pub async fn request(&self, mut req: Request<B>) -> anyhow::Result<hyper::Response<Incoming>> {
        let Some(intercept) = self.inner.matcher.intercept(req.uri()) else {
            return Ok(self.inner.direct.request(req).await?);
        };
        let is_https = req.uri().scheme_str() == Some("https");
        if req.uri().scheme_str() == Some("http") && req.uri().port().is_none() {
            *req.uri_mut() = with_default_port(req.uri().clone(), 80)?;
        }
        let slot = if is_https { &self.inner.https_proxy } else { &self.inner.http_proxy };
        let proxy = match slot.get() {
            Some(proxy) => proxy,
            None => {
                let built = ProxyClient::new(&intercept, self.inner.connect_timeout)?;
                let _ = slot.set(built);
                slot.get().expect("proxy client just initialized")
            }
        };
        proxy.request(req).await
    }
}

impl<B> ProxyClient<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn new(intercept: &Intercept, connect_timeout: Option<Duration>) -> anyhow::Result<Self> {
        let scheme = intercept.uri().scheme_str().unwrap_or("http");
        let roots = HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("the machine's certificates cannot be read")?
            .https_or_http()
            .enable_http1();
        match scheme {
            "http" | "https" => {
                let mut tunnel =
                    Tunnel::new(intercept.uri().clone(), https_connector(connect_timeout)?);
                if let Some(auth) = intercept.basic_auth() {
                    tunnel = tunnel.with_auth(auth.clone());
                }
                let connector = roots.wrap_connector(tunnel);
                Ok(ProxyClient::Tunnel(Client::builder(TokioExecutor::new()).build(connector)))
            }
            "socks5" | "socks5h" => {
                let proxy_uri = with_default_port(intercept.uri().clone(), 1080)?;
                let mut socks = SocksV5::new(proxy_uri, http_connector(connect_timeout))
                    .local_dns(scheme == "socks5");
                if let Some((user, pass)) = intercept.raw_auth() {
                    socks = socks.with_auth(user.to_owned(), pass.to_owned());
                }
                let connector = roots.wrap_connector(socks);
                Ok(ProxyClient::SocksV5(Client::builder(TokioExecutor::new()).build(connector)))
            }
            "socks4" | "socks4a" => {
                if intercept.raw_auth().is_some() {
                    bail!(
                        "socks4 proxy `{}` does not support username/password authentication; \
                         use socks5:// instead",
                        intercept.uri()
                    );
                }
                let proxy_uri = with_default_port(intercept.uri().clone(), 1080)?;
                let socks = SocksV4::new(proxy_uri, http_connector(connect_timeout))
                    .local_dns(scheme == "socks4");
                let connector = roots.wrap_connector(socks);
                Ok(ProxyClient::SocksV4(Client::builder(TokioExecutor::new()).build(connector)))
            }
            other => bail!("unsupported proxy scheme `{other}` in `{}`", intercept.uri()),
        }
    }

    async fn request(&self, req: Request<B>) -> anyhow::Result<hyper::Response<Incoming>> {
        let response = match self {
            ProxyClient::Tunnel(client) => client.request(req).await?,
            ProxyClient::SocksV5(client) => client.request(req).await?,
            ProxyClient::SocksV4(client) => client.request(req).await?,
        };
        Ok(response)
    }
}

fn http_connector(connect_timeout: Option<Duration>) -> HttpConnector {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(connect_timeout);
    http
}

fn https_connector(
    connect_timeout: Option<Duration>,
) -> anyhow::Result<HttpsConnector<HttpConnector>> {
    let roots = HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("the machine's certificates cannot be read")?;
    Ok(roots.https_or_http().enable_http1().wrap_connector(http_connector(connect_timeout)))
}

fn with_default_port(uri: Uri, default_port: u16) -> anyhow::Result<Uri> {
    if uri.port().is_some() {
        return Ok(uri);
    }
    let host = uri.host().context("URI has no host")?;
    let authority = if host.starts_with('[') || !host.contains(':') {
        format!("{host}:{default_port}")
    } else {
        format!("[{host}]:{default_port}")
    };
    let mut parts = uri.into_parts();
    parts.authority = Some(authority.parse()?);
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some(hyper::http::uri::PathAndQuery::from_static("/"));
    }
    Ok(Uri::from_parts(parts)?)
}

fn validate_proxy_var(name: &str, val: &str) -> anyhow::Result<()> {
    if val.is_empty() {
        return Ok(());
    }
    let probe = Uri::from_static("https://example.com");
    let Some(intercept) = Matcher::builder().all(val).build().intercept(&probe) else {
        bail!(
            "{name} is not a supported proxy address \
             (expected http://, https://, socks5://, socks5h://, socks4:// or socks4a://)"
        );
    };
    if matches!(intercept.uri().scheme_str(), Some("socks4" | "socks4a"))
        && intercept.raw_auth().is_some()
    {
        bail!(
            "{name}: socks4 proxy does not support username/password authentication; \
             use socks5:// instead"
        );
    }
    Ok(())
}

fn matcher_from_env() -> anyhow::Result<Matcher> {
    matcher_from_env_with(|name| std::env::var_os(name), cfg!(windows))
}

// Inject reads so tests do not mutate the process environment while other clients use it.
fn matcher_from_env_with(
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
    case_insensitive_names: bool,
) -> anyhow::Result<Matcher> {
    let first = |names: &[&'static str]| -> Option<(&'static str, String)> {
        names.iter().find_map(|&name| {
            let val = get(name)?.into_string().ok()?;
            let trimmed = val.trim();
            (!trimmed.is_empty()).then(|| (name, trimmed.to_owned()))
        })
    };
    let no = first(&["no_proxy", "NO_PROXY"]).map(|(_, v)| v).unwrap_or_default();
    // Presence matters: Cargo's empty override explicitly disables inherited proxies.
    if let Some(cargo) = get("CARGO_HTTP_PROXY") {
        let cargo = cargo.to_str().context("CARGO_HTTP_PROXY is not valid Unicode")?.trim();
        validate_proxy_var("CARGO_HTTP_PROXY", cargo)?;
        return Ok(Matcher::builder().all(cargo).no(no).build());
    }
    let all = first(&["all_proxy", "ALL_PROXY"]);
    // CGI maps the incoming Proxy header to HTTP_PROXY; it is not trusted config.
    // Windows aliases lowercase lookups to that same untrusted variable.
    let http = if get("REQUEST_METHOD").is_some() {
        if case_insensitive_names { None } else { first(&["http_proxy"]) }
    } else {
        first(&["http_proxy", "HTTP_PROXY"])
    };
    let https = first(&["https_proxy", "HTTPS_PROXY"]);
    if let Some((name, val)) = &http {
        validate_proxy_var(name, val)?;
    }
    if let Some((name, val)) = &https {
        validate_proxy_var(name, val)?;
    }
    if (http.is_none() || https.is_none())
        && let Some((name, val)) = &all
    {
        validate_proxy_var(name, val)?;
    }
    Ok(Matcher::builder()
        .all(all.map(|(_, v)| v).unwrap_or_default())
        .http(http.map(|(_, v)| v).unwrap_or_default())
        .https(https.map(|(_, v)| v).unwrap_or_default())
        .no(no)
        .build())
}

/// How many redirects one request follows.
const REDIRECTS: usize = 10;
/// How many requests [`HyperHttp::get_async`] has in flight at once, each on a connection of
/// its own (HTTP/1).
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
    client: HttpClient<Full<Bytes>>,
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
        let client = HttpClient::new(None)?;
        // No version: this crate is under every build of blaze, and the release blaze is is told
        // only to the engine (its build.rs), so that a release changes no crate below it.
        let user_agent = "blaze (https://github.com/indexable-inc/blaze)".into();
        Ok(HyperHttp {
            runtime,
            client: Requests { client, user_agent, timeout },
            in_flight: Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT)),
        })
    }

    /// One shared by the whole process, with cargo's default timeout of 30 seconds.
    pub fn shared() -> anyhow::Result<Arc<HyperHttp>> {
        static SHARED: OnceLock<Result<Arc<HyperHttp>, String>> = OnceLock::new();
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
        self.runtime.block_on(self.client.fetch(method, url, headers, Bytes::from(body)))
    }
}

impl Requests {
    async fn fetch(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> anyhow::Result<Response> {
        let mut address = url.to_owned();
        for _ in 0..=REDIRECTS {
            let mut request = Request::builder()
                .method(method.clone())
                .uri(&address)
                .header(header::USER_AGENT, &*self.user_agent)
                .header(header::ACCEPT_ENCODING, "gzip");
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let request = request.body(Full::new(body.clone()))?;
            let answer = tokio::time::timeout(self.timeout, self.client.request(request))
                .await
                .map_err(|_| Transport(format!("no answer from {address} in {:?}", self.timeout)))?
                .map_err(|e| Transport(format!("asking {address}: {e:#}")))?;
            let status = answer.status();
            if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
                let to = answer.headers().get(header::LOCATION).and_then(|it| it.to_str().ok());
                let to = to.with_context(|| format!("{address} redirects nowhere"))?;
                address = url::Url::parse(&address)?.join(to)?.to_string();
                continue;
            }
            let headers: Vec<(String, String)> = answer
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
                })
                .collect();
            let mut body = answer.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = self.next(&mut body, &address).await? {
                bytes.extend_from_slice(&chunk);
            }
            let mut response = Response { status: status.as_u16(), headers, body: bytes };
            if response.header("content-encoding").is_some_and(|it| it.eq_ignore_ascii_case("gzip"))
            {
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(&response.body[..])
                    .read_to_end(&mut decoded)
                    .map_err(|e| Transport(format!("{address} sent gzip that is not: {e}")))?;
                response.body = decoded;
            }
            return Ok(response);
        }
        anyhow::bail!("{url}: more than {REDIRECTS} redirects")
    }

    async fn next(&self, body: &mut Incoming, address: &str) -> anyhow::Result<Option<Bytes>> {
        loop {
            let frame = tokio::time::timeout(self.timeout, body.frame()).await.map_err(|_| {
                Transport(format!("transfer from {address} stalled for {:?}", self.timeout))
            })?;
            let Some(frame) = frame else { return Ok(None) };
            let frame = frame.map_err(|e| Transport(format!("reading from {address}: {e}")))?;
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

    fn get_async(&self, url: &str, headers: &[(&str, &str)]) -> Pending<anyhow::Result<Response>> {
        let (client, in_flight) = (self.client.clone(), self.in_flight.clone());
        let url = url.to_owned();
        let headers: Vec<(String, String)> =
            headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        let task = self.runtime.spawn(async move {
            let _turn = in_flight.acquire_owned().await?;
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
            client.fetch(Method::GET, &url, &headers, Bytes::new()).await
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn retries_what_may_pass() {
        let retry = Retry { retries: 2, quiet: true, fixed_sleep_ms: Some(0) };
        let tries = AtomicU32::new(0);
        let answer = Response { status: 500, ..Response::default() };
        let failed: anyhow::Result<()> = retry.run(|| {
            tries.fetch_add(1, Ordering::Relaxed);
            Err(HttpNotSuccessful::new("u", &answer).into())
        });
        assert!(failed.is_err());
        assert_eq!(tries.load(Ordering::Relaxed), 3);

        let tries = AtomicU32::new(0);
        let answer = Response { status: 403, ..Response::default() };
        let failed: anyhow::Result<()> = retry.run(|| {
            tries.fetch_add(1, Ordering::Relaxed);
            Err(HttpNotSuccessful::new("u", &answer).into())
        });
        assert!(failed.is_err());
        assert_eq!(tries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sleeps_as_cargo() {
        assert!((500..1500).contains(&sleep_ms(1)));
        assert_eq!(sleep_ms(2), 3500);
        assert_eq!(sleep_ms(3), 6500);
        assert_eq!(sleep_ms(4), 9500);
        assert_eq!(sleep_ms(5), 10000);
        let answer = |code, after: &str| HttpNotSuccessful {
            url: String::new(),
            code,
            retry_after: Some(after.to_owned()),
            body: String::new(),
        };
        assert_eq!(retry_after(&answer(429, "2")), Some(2000));
        assert_eq!(retry_after(&answer(500, "2")), None);
    }

    #[test]
    fn adds_default_ports_to_uris() {
        let http: Uri = "http://example.com/a/b?c=1".parse().unwrap();
        assert_eq!(
            with_default_port(http, 80).unwrap().to_string(),
            "http://example.com:80/a/b?c=1"
        );
        let explicit: Uri = "http://example.com:8080/a".parse().unwrap();
        assert_eq!(
            with_default_port(explicit, 80).unwrap().to_string(),
            "http://example.com:8080/a"
        );
        let ipv6: Uri = "socks5://[::1]/".parse().unwrap();
        assert_eq!(with_default_port(ipv6, 1080).unwrap().to_string(), "socks5://[::1]:1080/");
    }

    #[test]
    fn proxy_errors_do_not_disclose_credentials() {
        for value in [
            "ftp://private-user:private-password@proxy.example",
            "http://private-user:private-password@proxy.example:bad port",
            "socks4://private-user:private-password@proxy.example",
            "socks4a://private-user:private-password@proxy.example",
        ] {
            let error = validate_proxy_var("HTTPS_PROXY", value).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("HTTPS_PROXY"));
            assert!(!message.contains("private-user"));
            assert!(!message.contains("private-password"));
        }
    }

    fn proxy_env(vars: &[(&str, &str)]) -> anyhow::Result<Matcher> {
        proxy_env_for_platform(vars, false)
    }

    fn proxy_env_for_platform(
        vars: &[(&str, &str)],
        case_insensitive_names: bool,
    ) -> anyhow::Result<Matcher> {
        matcher_from_env_with(
            |name| {
                vars.iter()
                    .find(|(key, _)| {
                        if case_insensitive_names {
                            key.eq_ignore_ascii_case(name)
                        } else {
                            *key == name
                        }
                    })
                    .map(|(_, value)| (*value).into())
            },
            case_insensitive_names,
        )
    }

    #[test]
    fn empty_cargo_proxy_disables_inherited_proxies() {
        for case_insensitive_names in [false, true] {
            for cgi in [false, true] {
                let mut vars = vec![
                    ("CARGO_HTTP_PROXY", ""),
                    ("HTTP_PROXY", "http://inherited.example"),
                    ("HTTPS_PROXY", "http://inherited.example"),
                    ("ALL_PROXY", "ftp://invalid.example"),
                ];
                if cgi {
                    vars.push(("REQUEST_METHOD", "GET"));
                }
                let matcher = proxy_env_for_platform(&vars, case_insensitive_names).unwrap();
                for uri in ["http://origin.example", "https://origin.example"] {
                    assert!(matcher.intercept(&uri.parse().unwrap()).is_none());
                }
            }
            // An absent override still honors inherited proxies.
            let matcher = proxy_env_for_platform(
                &[("HTTPS_PROXY", "http://inherited.example")],
                case_insensitive_names,
            )
            .unwrap();
            assert!(matcher.intercept(&Uri::from_static("https://origin.example")).is_some());
        }
    }

    #[test]
    fn windows_cgi_ignores_http_proxy_aliases() {
        for alias in ["HTTP_PROXY", "http_proxy", "Http_Proxy"] {
            let mut vars = vec![("REQUEST_METHOD", "GET"), (alias, "http://untrusted.example")];
            let matcher = proxy_env_for_platform(&vars, true).unwrap();
            assert!(matcher.intercept(&Uri::from_static("http://origin.example")).is_none());
            // Trusted settings remain usable even with the untrusted HTTP alias present.
            for (name, uri) in [
                ("CARGO_HTTP_PROXY", "http://origin.example"),
                ("CARGO_HTTP_PROXY", "https://origin.example"),
                ("HTTPS_PROXY", "https://origin.example"),
                ("ALL_PROXY", "http://origin.example"),
                ("ALL_PROXY", "https://origin.example"),
            ] {
                vars.push((name, "http://trusted.example"));
                let matcher = proxy_env_for_platform(&vars, true).unwrap();
                assert_eq!(
                    matcher.intercept(&uri.parse().unwrap()).unwrap().uri().host(),
                    Some("trusted.example")
                );
                vars.pop();
            }
        }
    }

    #[test]
    fn lowercase_proxy_settings_win() {
        for (lower, upper, scheme) in [
            ("http_proxy", "HTTP_PROXY", "http"),
            ("https_proxy", "HTTPS_PROXY", "https"),
            ("all_proxy", "ALL_PROXY", "http"),
            ("all_proxy", "ALL_PROXY", "https"),
        ] {
            let matcher = proxy_env(&[
                (lower, "http://preferred.example:8080"),
                (upper, "ftp://invalid.example"),
            ])
            .unwrap();
            let uri: Uri = format!("{scheme}://origin.example").parse().unwrap();
            assert_eq!(matcher.intercept(&uri).unwrap().uri().host(), Some("preferred.example"));
        }
        let matcher = proxy_env(&[
            ("all_proxy", "http://proxy.example"),
            ("no_proxy", "lower.example"),
            ("NO_PROXY", "upper.example"),
        ])
        .unwrap();
        assert!(matcher.intercept(&Uri::from_static("https://lower.example")).is_none());
        assert!(matcher.intercept(&Uri::from_static("https://upper.example")).is_some());
    }

    #[test]
    fn cgi_keeps_trusted_proxy_settings() {
        for (name, scheme) in [
            ("CARGO_HTTP_PROXY", "http"),
            ("CARGO_HTTP_PROXY", "https"),
            ("http_proxy", "http"),
            ("HTTPS_PROXY", "https"),
            ("https_proxy", "https"),
            ("ALL_PROXY", "http"),
            ("ALL_PROXY", "https"),
            ("all_proxy", "http"),
            ("all_proxy", "https"),
        ] {
            let matcher = proxy_env(&[
                ("REQUEST_METHOD", "GET"),
                ("HTTP_PROXY", "ftp://untrusted.example"),
                (name, "http://trusted.example:8080"),
            ])
            .unwrap();
            let uri: Uri = format!("{scheme}://origin.example").parse().unwrap();
            assert_eq!(matcher.intercept(&uri).unwrap().uri().host(), Some("trusted.example"));
        }
        let matcher =
            proxy_env(&[("REQUEST_METHOD", ""), ("HTTP_PROXY", "http://untrusted.example")])
                .unwrap();
        assert!(matcher.intercept(&Uri::from_static("http://origin.example")).is_none());
        assert!(
            proxy_env(&[("REQUEST_METHOD", "GET"), ("HTTPS_PROXY", "ftp://invalid.example")])
                .is_err()
        );
    }

    #[test]
    fn cargo_proxy_override_and_no_proxy_work_in_cgi() {
        let matcher = proxy_env(&[
            ("REQUEST_METHOD", "GET"),
            ("CARGO_HTTP_PROXY", "http://cargo.example:8080"),
            ("http_proxy", "ftp://invalid.example"),
            ("https_proxy", "ftp://invalid.example"),
            ("all_proxy", "ftp://invalid.example"),
            ("no_proxy", "direct.example"),
        ])
        .unwrap();
        for scheme in ["http", "https"] {
            let uri: Uri = format!("{scheme}://origin.example").parse().unwrap();
            assert_eq!(matcher.intercept(&uri).unwrap().uri().host(), Some("cargo.example"));
            let direct: Uri = format!("{scheme}://direct.example").parse().unwrap();
            assert!(matcher.intercept(&direct).is_none());
        }
        let matcher = proxy_env(&[("HTTP_PROXY", "http://uppercase.example")]).unwrap();
        assert_eq!(
            matcher.intercept(&Uri::from_static("http://origin.example")).unwrap().uri().host(),
            Some("uppercase.example")
        );
    }

    #[test]
    fn validates_proxy_addresses() {
        assert!(validate_proxy_var("HTTPS_PROXY", "http://127.0.0.1:7890").is_ok());
        assert!(validate_proxy_var("HTTPS_PROXY", "127.0.0.1:7890").is_ok());
        assert!(validate_proxy_var("ALL_PROXY", "socks5h://user:pass@127.0.0.1:1080").is_ok());
        assert!(validate_proxy_var("ALL_PROXY", "socks4a://127.0.0.1:1080").is_ok());
        assert!(validate_proxy_var("ALL_PROXY", "socks4://user:pass@127.0.0.1:1080").is_err());
        assert!(validate_proxy_var("HTTPS_PROXY", "ftp://127.0.0.1:21").is_err());
        assert!(validate_proxy_var("HTTPS_PROXY", "://not-a-uri").is_err());
    }
}
