//! The thin HTTP driver: a [`Bucket`] over blaze-http's client on its tokio driver, on a
//! runtime of its own. It does IO and nothing else: what to ask is the machines'.
//!
//! - A read (GET, HEAD) of a public object goes to the bucket's public address unsigned, with
//!   no credential at all: the public cache is read anonymously.
//! - Everything else (every write, and reads of an organisation's namespace) goes to the S3
//!   endpoint, signed ([`crate::s3::sign`]); with no endpoint or credentials it is answered
//!   [`Reply::Failed`] naming the setting, and nothing is sent.
//! - Each ask runs under its own deadline and is answered [`Reply::TimedOut`] past it. Nothing
//!   is asked again, and redirects are not followed (an S3 answer never needs one).
//! - No header, address with a query, or body is ever logged or put in a reply: a failure says
//!   the object, the status and the client's kind of error.
//!
//! The wait for answers ([`Bucket::wait`]) opens no span yet: blaze-trace is not in this
//! crate's graph, like blaze-http's own waits. Each wait is bounded by the asks' deadlines.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_http::{
    Client, Config, Event, Request,
    tokio_driver::{Answer, Driver},
};
use tokio::sync::mpsc;

use crate::{
    bucket::{Asked, Bucket, Cond, Counts, Failure, Op, Reply, Ticket},
    s3,
    settings::{Credentials, Settings},
};

/// The most bytes an S3 answer that is not an object may be (XML of a multipart upload).
pub const XML_MOST: u64 = 1 << 20;

/// Where requests go and with what.
#[derive(Clone, Debug)]
pub struct Access {
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub public_url: Option<String>,
    pub credentials: Option<Credentials>,
}

/// What an answer is read as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    Get { ranged: bool },
    Head,
    Put,
    Begin,
    Part,
    Finish,
}

fn cond_header(cond: &Cond) -> Vec<(&'static str, String)> {
    match cond {
        Cond::Any => Vec::new(),
        Cond::Absent => vec![("if-none-match", "*".to_owned())],
        Cond::Matches(etag) => vec![("if-match", etag.clone())],
    }
}

/// The request for `op` at `unix_s`, and how to read its answer. Why not, naming the setting
/// missing.
pub fn request(access: &Access, op: Op, unix_s: u64) -> Result<(Request, Want), String> {
    let anonymous = matches!(op, Op::Get { .. } | Op::Head { .. }) && op.name().starts_with("public/");
    if anonymous && let Some(public_url) = &access.public_url {
        let url = s3::public(public_url, op.name());
        let (method, want, most, headers) = match op {
            Op::Get { range: Some((at, len)), most, .. } => (http::Method::GET, Want::Get { ranged: true }, most.min(len), vec![("range".to_owned(), format!("bytes={at}-{}", at + len.max(1) - 1))]),
            Op::Get { most, .. } => (http::Method::GET, Want::Get { ranged: false }, most, Vec::new()),
            _ => (http::Method::HEAD, Want::Head, 0, Vec::new()),
        };
        return Ok((Request { method, url, headers, body: Vec::new(), gzip: false, most, https_only: true }, want));
    }
    let (Some(endpoint), Some(bucket), Some(credentials)) = (&access.endpoint, &access.bucket, &access.credentials) else {
        let mut missing = Vec::new();
        if access.endpoint.is_none() {
            missing.push("bucket.endpoint (BLAZE_CACHE_ENDPOINT)");
        }
        if access.bucket.is_none() {
            missing.push("bucket.name (BLAZE_CACHE_BUCKET)");
        }
        if access.credentials.is_none() {
            missing.push("the access key (BLAZE_CACHE_ACCESS_KEY_ID, BLAZE_CACHE_SECRET_ACCESS_KEY)");
        }
        if anonymous {
            missing.push("or bucket.public_url (BLAZE_CACHE_PUBLIC_URL) for anonymous reads");
        }
        return Err(format!("{}: a signed request needs {}", op.name(), missing.join(", ")));
    };
    let name = op.name().to_owned();
    let sign = |method: &str, query: &[(&str, &str)], extra: &[(&str, String)], body: &[u8]| s3::sign(method, endpoint, bucket, &name, query, extra, body, credentials, unix_s);
    let (method, want, most, signed, body) = match op {
        Op::Get { range, most, .. } => {
            let extra: Vec<(&str, String)> = range.map(|(at, len)| ("range", format!("bytes={at}-{}", at + len.max(1) - 1))).into_iter().collect();
            let most = range.map_or(most, |(_, len)| most.min(len));
            (http::Method::GET, Want::Get { ranged: range.is_some() }, most, sign("GET", &[], &extra, b""), Vec::new())
        }
        Op::Head { .. } => (http::Method::HEAD, Want::Head, 0, sign("HEAD", &[], &[], b""), Vec::new()),
        Op::Put { bytes, cond, .. } => (http::Method::PUT, Want::Put, XML_MOST, sign("PUT", &[], &cond_header(&cond), &bytes), bytes),
        Op::Begin { .. } => (http::Method::POST, Want::Begin, XML_MOST, sign("POST", &[("uploads", "")], &[], b""), Vec::new()),
        Op::Part { upload, number, bytes, .. } => {
            let number = number.to_string();
            (http::Method::PUT, Want::Part, XML_MOST, sign("PUT", &[("partNumber", number.as_str()), ("uploadId", upload.as_str())], &[], &bytes), bytes)
        }
        Op::Finish { upload, parts, cond, .. } => {
            let body = s3::complete_body(&parts);
            (http::Method::POST, Want::Finish, XML_MOST, sign("POST", &[("uploadId", upload.as_str())],&cond_header(&cond), &body), body)
        }
    };
    // The client sends its own host header; the signed one is the same value.
    let headers = signed.headers.into_iter().filter(|(k, _)| k != "host").collect();
    Ok((Request { method, url: signed.url, headers, body, gzip: false, most, https_only: true }, want))
}

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// The reply to an answer of `status` with `headers` and `body`, read as `want`, for `object`.
pub fn reply(object: &str, want: Want, status: u16, headers: &[(String, String)], body: Vec<u8>) -> Reply {
    let etag = header(headers, "etag").unwrap_or("").to_owned();
    match (want, status) {
        (_, 404) => Reply::Missing,
        (Want::Put | Want::Finish, 412) => Reply::Precondition,
        (Want::Get { ranged: false }, 200) | (Want::Get { ranged: true }, 206) => Reply::Got { bytes: body, etag },
        (Want::Get { ranged: true }, 200) => Reply::Failed { why: format!("{object}: a ranged read answered with the whole object") },
        (Want::Head, 200) => match header(headers, "content-length").and_then(|it| it.parse().ok()) {
            Some(len) => Reply::Exists { len },
            None => Reply::Failed { why: format!("{object}: an answer to HEAD with no length") },
        },
        (Want::Put | Want::Part, 200) if !etag.is_empty() => Reply::Done { etag },
        (Want::Finish, 200) if !s3::is_error(&body) => Reply::Done { etag },
        (Want::Finish, 200) => Reply::Failed { why: format!("{object}: completing the upload answered an error") },
        (Want::Begin, 200) => match s3::upload_id(&body) {
            Some(upload) => Reply::Begun { upload },
            None => Reply::Failed { why: format!("{object}: starting an upload answered no upload id") },
        },
        (_, status) => Reply::Failed { why: format!("{object}: HTTP {status}") },
    }
}

async fn exchange(mut answer: Answer, object: String, want: Want) -> Reply {
    let (mut status, mut headers, mut body) = (0, Vec::new(), Vec::new());
    loop {
        match answer.next().await {
            Some(Event::Head { status: s, headers: h, .. }) => {
                status = s;
                headers = h;
            }
            Some(Event::Body { bytes, .. }) => body.extend_from_slice(&bytes),
            Some(Event::Done { .. }) => return reply(&object, want, status, &headers, body),
            // The error's text is its kind: never its address or headers.
            Some(Event::Failed { error, .. }) => return Reply::Failed { why: format!("{object}: {error}") },
            None => return Reply::Failed { why: format!("{object}: the HTTP driver ended") },
        }
    }
}

/// The bucket over HTTPS.
pub struct HttpBucket {
    runtime: tokio::runtime::Runtime,
    driver: Driver,
    access: Arc<Access>,
    start: Instant,
    to_us: mpsc::UnboundedSender<(Ticket, Reply)>,
    from_tasks: mpsc::UnboundedReceiver<(Ticket, Reply)>,
    in_flight: usize,
    counts: Counts,
}

impl HttpBucket {
    /// The bucket of `settings`; [`Failure::NotConfigured`] when the cache contacts nothing.
    pub fn new(settings: &Settings) -> Result<HttpBucket, Failure> {
        let Some(bucket) = settings.contacts() else {
            return Err(Failure::NotConfigured { missing: vec!["the cache is not configured or is disabled (remote.json)".to_owned()] });
        };
        let access = Access { endpoint: bucket.endpoint.clone(), bucket: bucket.name.clone(), public_url: bucket.public_url.clone(), credentials: settings.credentials.clone() };
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("blaze-remote http").enable_all().build().map_err(|e| Failure::Misused(format!("making the cache's HTTP runtime: {e}")))?;
        let tls = blaze_http::roots::native().map_err(|e| Failure::Misused(format!("the machine's TLS roots: {e}")))?;
        let proxies = blaze_http::proxy_env::from_env().map_err(|e| Failure::Misused(format!("the proxy settings: {e}")))?;
        let config = Config { idle_most: crate::push::IN_FLIGHT, proxies, redirects: 0, ..Config::new(tls, "blaze (https://github.com/indexable-inc/blaze)", Duration::from_secs(30)) };
        let driver = Driver::spawn(runtime.handle(), Client::new(config));
        let (to_us, from_tasks) = mpsc::unbounded_channel();
        // alloc: the driver's settings, made once with it.
        Ok(HttpBucket { runtime, driver, access: Arc::new(access), start: Instant::now(), to_us, from_tasks, in_flight: 0, counts: Counts::default() })
    }
}

impl Bucket for HttpBucket {
    fn now_ms(&mut self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn send(&mut self, asked: Vec<Asked>) {
        let now = self.now_ms();
        let unix_s = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_secs());
        for Asked { ticket, op, deadline_ms } in asked {
            self.in_flight += 1;
            self.counts.requests += 1;
            self.counts.sent += op.sends();
            let object = op.name().to_owned();
            let to_us = self.to_us.clone();
            match request(&self.access, op, unix_s) {
                Err(why) => {
                    _ = to_us.send((ticket, Reply::Failed { why }));
                }
                Ok((req, want)) => {
                    let answer = self.driver.ask(req);
                    let allowed = Duration::from_millis(deadline_ms.saturating_sub(now));
                    self.runtime.spawn(async move {
                        let reply = tokio::time::timeout(allowed, exchange(answer, object, want)).await.unwrap_or(Reply::TimedOut);
                        _ = to_us.send((ticket, reply));
                    });
                }
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn wait(&mut self) -> Vec<(Ticket, Reply)> {
        if self.in_flight == 0 {
            return Vec::new();
        }
        // Bounded: every ask's task answers by its deadline. We hold a sender, so `None`
        // cannot come; it is read as nothing answered.
        let first = self.runtime.block_on(self.from_tasks.recv());
        let mut out: Vec<(Ticket, Reply)> = first.into_iter().collect();
        while let Ok(it) = self.from_tasks.try_recv() {
            out.push(it);
        }
        self.in_flight -= out.len();
        for (_, reply) in &out {
            if let Reply::Got { bytes, .. } = reply {
                self.counts.received += bytes.len() as u64;
            }
        }
        out
    }

    fn counts(&self) -> Counts {
        self.counts
    }
}
