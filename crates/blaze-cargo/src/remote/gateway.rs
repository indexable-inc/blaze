//! blaze's cache gateway (tools/remote-cache/API-v0.md): a tenant's objects at
//! `<base>/<name>`, read with GET and HEAD and written with PUT, behind a bearer token.
//!
//! `BLAZE_REMOTE=blaze+https://<gateway>/t/<tenant>/v3` names it (`blaze+http://` for a
//! gateway on this machine). The token is `BLAZE_REMOTE_TOKEN`, or the variable that
//! `?token=env:NAME` names: never the address itself, which is printed. Without one the
//! tenant is read anonymously, which a public tenant allows, and nothing is written.
//!
//! What a request is and what its answer means are worked out by the functions without IO
//! below ([`Ask`], [`judge`]); [`Gateway::send`] is the one place that talks HTTP.

use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use http_body_util::Full;
use hyper::{
    HeaderMap, Method, Request, StatusCode,
    body::{Bytes, Incoming},
    header,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_derive::Deserialize;
use url::Url;

use super::{
    s3::Written,
    transfer::{Body, Digest, Object, Refused, TooLong, Traffic, Version},
};

/// The variable the token is read from when the address names none.
const TOKEN_VAR: &str = "BLAZE_REMOTE_TOKEN";
/// The most one PUT carries; a larger object goes up in parts.
pub const PUT_MAX: u64 = 90 << 20;
/// The size of a part: the gateway takes none above [`PUT_MAX`] or below 5 MiB but the last.
const PART: u64 = 64 << 20;
/// How many parts of one object are in flight: each is held in memory while it goes.
const PARTS_AT_ONCE: usize = 4;
/// How often a request is made again after a 429, each after the wait the gateway asks for.
const RATE_LIMITED: u32 = 5;
/// The longest wait a 429 is taken at its word for.
const LONGEST_WAIT: Duration = Duration::from_secs(60);
/// An upload is given up when it is slower than this for the whole of an attempt.
const SLOWEST_UPLOAD: u64 = 64 << 10;
/// The most of an error's body that is read and shown.
const REASON_MAX: usize = 300;

pub struct Gateway {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    spec: Spec,
    token: Option<String>,
    patience: Duration,
    traffic: Arc<Traffic>,
}

/// What a `blaze+` address says.
#[derive(Debug, PartialEq, Eq)]
pub struct Spec {
    /// `https://<gateway>/t/<tenant>/v3`, with no `/` at its end and no query.
    pub base: String,
    pub tenant: String,
    /// The variable the token is read from.
    pub token_var: String,
}

impl Spec {
    pub fn parse(spec: &str) -> anyhow::Result<Spec> {
        let plain =
            spec.strip_prefix("blaze+").context("a gateway's address starts with blaze+")?;
        let url = Url::parse(plain).context("the address of the gateway is not a URL")?;
        if !matches!(url.scheme(), "https" | "http") {
            bail!("a gateway is reached over blaze+https:// or blaze+http://, not `{spec}`");
        }
        let mut token_var = TOKEN_VAR.to_owned();
        for (name, value) in url.query_pairs() {
            match (&*name, value.strip_prefix("env:")) {
                ("token", Some(var)) if !var.is_empty() => token_var = var.to_owned(),
                ("token", _) => bail!(
                    "a gateway's token is named as `?token=env:NAME`, the variable it is in: \
                     the address is printed, and a token in it would be too"
                ),
                _ => bail!("`{name}` is not an option of a gateway's address"),
            }
        }
        let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
        let tenant = match segments.as_slice() {
            [.., "t", tenant, "v3"] if !tenant.is_empty() => (*tenant).to_owned(),
            _ => bail!("a gateway's address ends in /t/<tenant>/v3: `{plain}`"),
        };
        let mut base = url.clone();
        base.set_query(None);
        let base = base.as_str().trim_end_matches('/').to_owned();
        Ok(Spec { base, tenant, token_var })
    }
}

/// One request of the API, before it is sent.
#[derive(Debug)]
pub struct Ask {
    pub method: Method,
    pub op: &'static str,
    pub name: String,
    /// What follows `?`, if anything.
    pub query: Option<String>,
    pub headers: Vec<(header::HeaderName, String)>,
}

impl Ask {
    fn new(method: Method, op: &'static str, name: &str) -> Ask {
        Ask { method, op, name: name.to_owned(), query: None, headers: Vec::new() }
    }

    fn header(mut self, name: header::HeaderName, value: impl Into<String>) -> Ask {
        self.headers.push((name, value.into()));
        self
    }

    fn query(mut self, query: String) -> Ask {
        self.query = Some(query);
        self
    }

    pub fn get(name: &str, from: u64, to: Option<u64>) -> Ask {
        let ask = Ask::new(Method::GET, "read", name);
        match (from, to) {
            (0, None) => ask,
            (from, None) => ask.header(header::RANGE, format!("bytes={from}-")),
            (from, Some(to)) => ask.header(header::RANGE, format!("bytes={from}-{to}")),
        }
    }

    pub fn head(name: &str) -> Ask {
        Ask::new(Method::HEAD, "read", name)
    }

    /// A PUT of a whole object: `read_at` is the ETag it must still have, and `None` that
    /// there must be nothing there. An object whose name is not the hash of its bytes says
    /// what they hash to.
    pub fn put(name: &str, bytes: &[u8], read_at: Option<&str>) -> Ask {
        let ask = Ask::new(Method::PUT, "write", name);
        let ask = match read_at {
            Some(tag) => ask.header(header::IF_MATCH, tag),
            None => ask.header(header::IF_NONE_MATCH, "*"),
        };
        ask.sha256_if_packed(|| Digest::of(bytes))
    }

    fn sha256_if_packed(self, digest: impl FnOnce() -> Digest) -> Ask {
        match self.name.ends_with(".pack.zst") {
            true => {
                self.header(header::HeaderName::from_static("x-blaze-sha256"), digest().to_string())
            }
            false => self,
        }
    }

    pub fn start_upload(name: &str) -> Ask {
        Ask::new(Method::POST, "write", name).query("uploads".to_owned())
    }

    pub fn put_part(name: &str, upload: &str, part: u64) -> Ask {
        Ask::new(Method::PUT, "write", name).query(format!("upload={}&part={part}", escape(upload)))
    }

    pub fn complete(name: &str, upload: &str, digest: Digest) -> Ask {
        let ask = Ask::new(Method::POST, "write", name).query(format!("upload={}", escape(upload)));
        ask.header(header::CONTENT_TYPE, "application/json").sha256_if_packed(|| digest)
    }

    pub fn abort(name: &str, upload: &str) -> Ask {
        Ask::new(Method::DELETE, "write", name).query(format!("upload={}", escape(upload)))
    }

    fn address(&self, base: &str) -> String {
        match &self.query {
            Some(query) => format!("{base}/{}?{query}", self.name),
            None => format!("{base}/{}", self.name),
        }
    }
}

fn escape(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// What an answer means to the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Judged {
    /// The object, or what was written.
    Done,
    /// Nothing is there, or nothing this machine may know of.
    Absent,
    /// A conditional write whose condition did not hold: another machine wrote first.
    Lost,
    /// A range that starts at or past the end of the object.
    PastTheEnd,
    /// The gateway asks to be asked again after this long.
    Wait(Duration),
    /// A no that asking again does not change.
    Refused(String),
    /// A failure that waiting may cure.
    Failed(String),
}

/// What the answer `status` with `headers` to `ask` means. `reason` is the start of the
/// answer's body, which the gateway's errors say why in. `token_var` is where the token
/// was looked for, and `had_token` whether one was there.
pub fn judge(
    ask: &Ask,
    spec: &Spec,
    had_token: bool,
    status: StatusCode,
    headers: &HeaderMap,
    reason: &str,
) -> Judged {
    let what = format!("{} {} of tenant `{}`", ask.op, ask.name, spec.tenant);
    let reason = reason.lines().next().unwrap_or_default().trim();
    let because = if reason.is_empty() { String::new() } else { format!(": {reason}") };
    match status {
        StatusCode::OK | StatusCode::NO_CONTENT => Judged::Done,
        StatusCode::PARTIAL_CONTENT if ask.method == Method::GET => Judged::Done,
        StatusCode::NOT_FOUND => Judged::Absent,
        StatusCode::PRECONDITION_FAILED => Judged::Lost,
        StatusCode::RANGE_NOT_SATISFIABLE => Judged::PastTheEnd,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            let token = match had_token {
                true => {
                    format!("the token in {} is not one the gateway takes for it", spec.token_var)
                }
                false => format!(
                    "{} is not set, and the tenant is not read without a token",
                    spec.token_var
                ),
            };
            Judged::Refused(format!("the gateway refused {what} ({status}{because}): {token}"))
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let after = headers.get(header::RETRY_AFTER).and_then(|it| it.to_str().ok());
            let after = after.and_then(|it| it.trim().parse::<u64>().ok()).unwrap_or(1);
            Judged::Wait(Duration::from_secs(after).min(LONGEST_WAIT))
        }
        it if it.is_server_error() => {
            Judged::Failed(format!("the gateway failed {what}: {it}{because}"))
        }
        it => Judged::Refused(format!("the gateway refused {what}: {it}{because}")),
    }
}

/// The total length an answer to a ranged GET says the object has.
fn total_of(headers: &HeaderMap) -> Option<u64> {
    let range = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    range.rsplit_once('/')?.1.trim().parse().ok()
}

/// An answer as the client acts on it: what it means, and its headers and body.
struct Answer {
    judged: Judged,
    headers: HeaderMap,
    body: Option<Incoming>,
}

#[derive(Deserialize)]
struct Etagged {
    etag: String,
}

#[derive(Deserialize)]
struct Started {
    upload: String,
}

impl Gateway {
    pub fn open(spec: &str, patience: Duration, traffic: Arc<Traffic>) -> anyhow::Result<Gateway> {
        let parsed = Spec::parse(spec)?;
        let token = std::env::var(&parsed.token_var).ok().filter(|it| !it.is_empty());
        Gateway::with_token(spec, token, patience, traffic)
    }

    pub fn with_token(
        spec: &str,
        token: Option<String>,
        patience: Duration,
        traffic: Arc<Traffic>,
    ) -> anyhow::Result<Gateway> {
        let spec = Spec::parse(spec)?;
        let roots = HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("the machine's certificates cannot be read")?;
        let connector =
            roots.https_or_http().enable_http1().wrap_connector(super::https::connector(patience));
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Ok(Gateway { client, spec, token, patience, traffic })
    }

    pub fn place(&self) -> String {
        self.spec.base.clone()
    }

    /// Why nothing is written, when this machine has no token to write with.
    pub fn cannot_write(&self) -> Option<String> {
        self.token.is_none().then(|| {
            format!("{} is not set: nothing is sent to {}", self.spec.token_var, self.spec.base)
        })
    }

    /// Sends `ask` with `body` and judges the answer, waiting out a 429 a few times. A
    /// refusal and a failure are errors here, so what comes back is what the caller acts on.
    async fn send(&self, ask: &Ask, body: Bytes) -> anyhow::Result<Answer> {
        let address = ask.address(&self.spec.base);
        let mut waits = 0;
        loop {
            let mut request = Request::builder().method(ask.method.clone()).uri(&address);
            for (name, value) in &ask.headers {
                request = request.header(name, value);
            }
            if let Some(token) = &self.token {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let request = request.body(Full::new(body.clone()))?;
            self.traffic.operations.fetch_add(1, Ordering::Relaxed);
            let patience = self.patience + Duration::from_secs(body.len() as u64 / SLOWEST_UPLOAD);
            let answer = tokio::time::timeout(patience, self.client.request(request))
                .await
                .with_context(|| format!("no answer for {patience:?} from {address}"))?
                .with_context(|| format!("asking {address}"))?;
            let (parts, body) = answer.into_parts();
            let (reason, body) = match parts.status.is_success() || ask.method == Method::HEAD {
                true => (String::new(), Some(body)),
                false => (self.reason(body).await, None),
            };
            let judged =
                judge(ask, &self.spec, self.token.is_some(), parts.status, &parts.headers, &reason);
            match judged {
                Judged::Wait(_) if waits == RATE_LIMITED => {
                    bail!("{address}: still rate limited after waiting {waits} times")
                }
                Judged::Wait(wait) => {
                    waits += 1;
                    self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(wait).await;
                }
                Judged::Refused(why) => return Err(Refused(why).into()),
                Judged::Failed(why) => bail!(why),
                judged => return Ok(Answer { judged, headers: parts.headers, body }),
            }
        }
    }

    /// The object under `name` from `offset` on, or `None` when there is nothing under it.
    pub async fn read_from(&self, name: &str, offset: u64) -> anyhow::Result<Option<Body>> {
        let answer = self.send(&Ask::get(name, offset, None), Bytes::new()).await?;
        match answer.judged {
            Judged::Absent => Ok(None),
            Judged::PastTheEnd => Ok(Some(Body::Empty)),
            _ => Ok(Some(Body::Http(answer.body.context("an answer without a body")?))),
        }
    }

    /// The bytes `from..=to` of the object `name` and how long the object is, or `None`
    /// when there is nothing under it. A range past its end is no bytes.
    pub async fn range(
        &self,
        name: &str,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Option<(Vec<u8>, u64)>> {
        let answer = self.send(&Ask::get(name, from, Some(to)), Bytes::new()).await?;
        let total = total_of(&answer.headers);
        let mut body = match answer.judged {
            Judged::Absent => return Ok(None),
            Judged::PastTheEnd => return Ok(Some((Vec::new(), total.unwrap_or(from)))),
            _ => answer.body.context("an answer without a body")?,
        };
        let total = total.with_context(|| format!("{name}: a range came without its length"))?;
        let want = to.min(total.saturating_sub(1)).saturating_sub(from) + 1;
        let mut bytes = Vec::with_capacity(want as usize);
        loop {
            let chunk = tokio::time::timeout(self.patience, super::https::next(&mut body)).await;
            let chunk = chunk.with_context(|| format!("no bytes for {:?}", self.patience))??;
            let Some(chunk) = chunk else { break };
            self.traffic.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if (bytes.len() + chunk.len()) as u64 > want {
                return Err(TooLong { name: name.to_owned(), most: want }.into());
            }
            bytes.extend_from_slice(&chunk);
        }
        if (bytes.len() as u64) < want {
            bail!("{name}: {} bytes of the {want} asked for from {from} came", bytes.len());
        }
        Ok(Some((bytes, total)))
    }

    pub async fn read_small(
        &self,
        name: &str,
        most: u64,
    ) -> anyhow::Result<Option<(Vec<u8>, Version)>> {
        let answer = self.send(&Ask::get(name, 0, None), Bytes::new()).await?;
        let mut body = match answer.judged {
            Judged::Absent => return Ok(None),
            _ => answer.body.context("an answer without a body")?,
        };
        let etag = answer.headers.get(header::ETAG).and_then(|it| it.to_str().ok());
        let version = Version(etag.map(str::to_owned));
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout(self.patience, super::https::next(&mut body)).await;
            let chunk = chunk.with_context(|| format!("no bytes for {:?}", self.patience))??;
            let Some(chunk) = chunk else { break };
            if (bytes.len() + chunk.len()) as u64 > most {
                return Err(TooLong { name: name.to_owned(), most }.into());
            }
            self.traffic.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some((bytes, version)))
    }

    pub async fn found(&self, name: &str) -> anyhow::Result<Option<Object>> {
        let answer = self.send(&Ask::head(name), Bytes::new()).await?;
        if answer.judged == Judged::Absent {
            return Ok(None);
        }
        let number = |name| {
            let value =
                answer.headers.get(name).and_then(|it: &header::HeaderValue| it.to_str().ok());
            value.and_then(|it| it.trim().parse::<u64>().ok())
        };
        let len = number(header::CONTENT_LENGTH).unwrap_or(0);
        // An object the gateway gives no time for is taken to have been written now, which
        // is the reading that sends nothing again.
        let written = number(header::HeaderName::from_static("x-blaze-uploaded"))
            .map_or_else(SystemTime::now, |it| SystemTime::UNIX_EPOCH + Duration::from_secs(it));
        Ok(Some(Object { name: name.to_owned(), len, written }))
    }

    /// Writes an object if what is there is still the version that was read: `None` when
    /// nothing was there. One longer than a PUT carries goes up in parts, unconditionally:
    /// only a pack or a unit is that long, and its name is the hash of its bytes.
    pub async fn write_small(
        &self,
        name: &str,
        bytes: &[u8],
        read_at: Option<&Version>,
    ) -> anyhow::Result<Written> {
        let len = bytes.len() as u64;
        if len > PUT_MAX {
            let bytes = Bytes::copy_from_slice(bytes);
            let digest = Digest::of(&bytes);
            let part = |at: u64, size: u64| {
                let bytes = bytes.slice(at as usize..(at + size) as usize);
                async move { anyhow::Ok(bytes) }
            };
            self.send_in_parts(name, len, digest, part).await?;
            return Ok(Written::Done);
        }
        let tag = read_at.and_then(|it| it.0.as_deref());
        let answer = self.send(&Ask::put(name, bytes, tag), Bytes::copy_from_slice(bytes)).await?;
        match answer.judged {
            Judged::Lost => Ok(Written::Lost),
            Judged::Done => {
                self.traffic.sent.fetch_add(len, Ordering::Relaxed);
                Ok(Written::Done)
            }
            other => bail!("{name}: the gateway answered a write with {other:?}"),
        }
    }

    /// Puts the file under `name`, all of it or nothing. An object that is there already
    /// is the same bytes, as its name is their hash.
    pub async fn send_file(&self, name: &str, file: &Path, len: u64) -> anyhow::Result<()> {
        if len <= PUT_MAX {
            let bytes = Bytes::from(tokio::fs::read(file).await?);
            let ask = Ask::put(name, &bytes, None);
            let answer = self.send(&ask, bytes).await?;
            if answer.judged == Judged::Done {
                self.traffic.sent.fetch_add(len, Ordering::Relaxed);
            }
            return Ok(());
        }
        let from = file.to_owned();
        let digest = tokio::task::spawn_blocking(move || {
            use sha2::Digest as _;
            let mut hash = sha2::Sha256::new();
            std::io::copy(&mut std::fs::File::open(&from)?, &mut hash)?;
            std::io::Result::Ok(Digest::from(<[u8; 32]>::from(hash.finalize())))
        });
        let digest = digest.await??;
        let part = |at: u64, size: u64| async move {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(file).await?;
            file.seek(std::io::SeekFrom::Start(at)).await?;
            let mut bytes = vec![0; size as usize];
            file.read_exact(&mut bytes).await?;
            anyhow::Ok(Bytes::from(bytes))
        };
        self.send_in_parts(name, len, digest, part).await
    }

    /// 1. An upload is started.
    /// 2. The parts go up a few at a time, `part(offset, size)` giving the bytes of each.
    /// 3. The upload is completed, which the gateway stores under the name only if what it
    ///    was sent hashes to `digest`; or aborted when a part could not be sent.
    async fn send_in_parts<F>(
        &self,
        name: &str,
        len: u64,
        digest: Digest,
        part: impl Fn(u64, u64) -> F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<Bytes>>,
    {
        let started = self.send(&Ask::start_upload(name), Bytes::new()).await?;
        let started: Started = serde_json::from_slice(&self.body_of(started, name).await?)
            .with_context(|| format!("{name}: the gateway named no upload"))?;
        let upload = started.upload;
        let numbers = 1..=len.div_ceil(PART);
        let sent = stream::iter(numbers.map(|number| {
            let (at, upload, part) = ((number - 1) * PART, &upload, &part);
            async move {
                let bytes = part(at, PART.min(len - at)).await?;
                let size = bytes.len() as u64;
                let answer = self.send(&Ask::put_part(name, upload, number), bytes).await?;
                let etag: Etagged = serde_json::from_slice(&self.body_of(answer, name).await?)
                    .with_context(|| format!("{name}: part {number} came back without an etag"))?;
                self.traffic.sent.fetch_add(size, Ordering::Relaxed);
                anyhow::Ok(serde_json::json!({ "part": number, "etag": etag.etag }))
            }
        }))
        .buffered(PARTS_AT_ONCE.min(self.traffic.jobs()))
        .try_collect::<Vec<_>>()
        .await;
        let completed = match sent {
            Ok(parts) => {
                let body = serde_json::json!({ "parts": parts, "sha256": digest.to_string() });
                let body = Bytes::from(serde_json::to_vec(&body)?);
                self.send(&Ask::complete(name, &upload, digest), body).await.map(drop)
            }
            Err(e) => Err(e),
        };
        if completed.is_err()
            && let Err(e) = self.send(&Ask::abort(name, &upload), Bytes::new()).await
        {
            eprintln!("remote: an upload of {name} is left unfinished: {e:#}");
        }
        completed.with_context(|| format!("sending {name}"))
    }

    /// The whole of a small answer's body.
    async fn body_of(&self, answer: Answer, name: &str) -> anyhow::Result<Vec<u8>> {
        let mut body = answer.body.with_context(|| format!("{name}: an answer without a body"))?;
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout(self.patience, super::https::next(&mut body)).await;
            let chunk = chunk.with_context(|| format!("no bytes for {:?}", self.patience))??;
            let Some(chunk) = chunk else { return Ok(bytes) };
            if bytes.len() + chunk.len() > 1 << 16 {
                bail!("{name}: the gateway's answer is longer than one is");
            }
            bytes.extend_from_slice(&chunk);
        }
    }

    /// The start of an error's body, as text.
    async fn reason(&self, mut body: Incoming) -> String {
        let mut bytes = Vec::new();
        let read = async {
            while let Ok(Some(chunk)) = super::https::next(&mut body).await {
                bytes.extend_from_slice(&chunk);
                if bytes.len() >= REASON_MAX {
                    break;
                }
            }
        };
        _ = tokio::time::timeout(self.patience, read).await;
        String::from_utf8_lossy(&bytes[..bytes.len().min(REASON_MAX)]).into_owned()
    }
}

#[cfg(test)]
mod tests;
