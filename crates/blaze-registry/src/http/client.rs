//! [`HyperHttp`]: the [`Http`] blaze asks servers through, over hyper 1 and rustls with ring
//! (the `net` feature).

use std::{io::Read, sync::Arc, time::Duration};

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

use super::{Bounds, Http, Pending, Refused, Response, Transport};

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
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Bytes::new(), bounds))
    }
}

/// Where `address` redirecting to `to` (its `Location`, maybe relative) leads, held to
/// `bounds`: with `https_only`, a redirect off HTTPS is [`Refused::Downgraded`].
fn redirected(address: &str, to: &str, bounds: Bounds) -> anyhow::Result<String> {
    let next = url::Url::parse(address)?.join(to)?;
    if bounds.https_only && next.scheme() != "https" {
        return Err(Refused::Downgraded { from: address.to_owned(), to: next.to_string() }.into());
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
    ) -> anyhow::Result<Response> {
        let mut address = url.to_owned();
        if bounds.https_only && url::Url::parse(url)?.scheme() != "https" {
            return Err(Refused::NotHttps { url: url.to_owned() }.into());
        }
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
                address = redirected(&address, to, bounds)?;
                continue;
            }
            let headers: Vec<(String, String)> = answer
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
                })
                .collect();
            let too_big = |said: Option<u64>, decoded: bool| Refused::TooBig {
                url: address.clone(),
                most: bounds.most,
                said,
                decoded,
            };
            let said = answer
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|it| it.to_str().ok()?.trim().parse::<u64>().ok());
            if let Some(said) = said.filter(|it| *it > bounds.most) {
                return Err(too_big(Some(said), false).into());
            }
            let mut body = answer.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = self.next(&mut body, &address).await? {
                if bytes.len() as u64 + chunk.len() as u64 > bounds.most {
                    return Err(too_big(None, false).into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let mut response = Response { status: status.as_u16(), headers, body: bytes };
            if response.header("content-encoding").is_some_and(|it| it.eq_ignore_ascii_case("gzip"))
            {
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(&response.body[..])
                    .take(bounds.most.saturating_add(1))
                    .read_to_end(&mut decoded)
                    .map_err(|e| Transport(format!("{address} sent gzip that is not: {e}")))?;
                if decoded.len() as u64 > bounds.most {
                    return Err(too_big(None, true).into());
                }
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
            client.fetch(Method::GET, &url, &headers, Bytes::new(), Bounds::NONE).await
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
