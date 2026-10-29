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

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    header,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
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
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
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
        let roots = HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("the machine's certificates cannot be read")?;
        let connector = roots.https_or_http().enable_http1().build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
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
}
