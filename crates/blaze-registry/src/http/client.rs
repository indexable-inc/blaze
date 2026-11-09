//! [`HyperHttp`]: the [`Http`] blaze asks servers through (the `net` feature), on blaze-http:
//! its sans-IO HTTP/1.1 client (`blaze_http::Client`: connections kept between requests by
//! origin, redirects, gzip, TLS through rustls on ring with the machine's roots, every timeout
//! a deadline) driven by its tokio driver on a runtime of this client's own. Every request goes
//! through the proxy the environment names for its origin, as curl and cargo read it
//! (`blaze_http::proxy`), read once as the client is made: a variable that can't be used fails
//! the client, naming it.
//!
//! The name is the first client's, kept so that no caller changes; nothing under it is hyper
//! any more (no hyper, hyper-util, hyper-rustls or tokio-rustls: docs/v2/plan/design/no-tracing.md,
//! 6.1).

use std::{io::Read as _, sync::Arc, time::Duration};

use anyhow::Context as _;
use blaze_http::{
    Client, Config, Event, Kind, Request,
    tokio_driver::{Answer, Driver},
};
use http::Method;

use blaze_fetch::{redact, scrub};

use super::{Bounds, Http, Pending, Refused, Response, Sink, Stalled, Transport, Watch};

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
        let proxies = blaze_http::proxy_env::from_env()?;
        let config =
            Config { idle_most: IN_FLIGHT, proxies, ..Config::new(tls, user_agent, timeout) };
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
        static SHARED: std::sync::OnceLock<Result<Arc<HyperHttp>, String>> =
            std::sync::OnceLock::new();
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
        self.runtime.block_on(self.client.fetch(method, url, headers, body, Bounds::NONE))
    }

    /// [`Http::get`] of `url` held to `bounds`: a body past `bounds.most` stops being read, and
    /// with `bounds.https_only` a plain-HTTP address or redirect is refused ([`Refused`]).
    pub fn get_bounded(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        bounds: Bounds,
    ) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.fetch(Method::GET, url, headers, Vec::new(), bounds))
    }

    /// `url`'s body from byte `from` into `sink`, in order, each piece with its offset in the
    /// whole body (0 when the server sends it whole): in ranges, [`PARALLEL`] at once, held to
    /// `bounds`, said in blaze-fetch's downloads as `what`, with `watch` told its line every
    /// [`TICK`] and at its end. No byte for the stall bound is [`Stalled`]. A 200 or 206 is
    /// success; any other answer is given with the first 64 KiB of its body, for its error.
    #[expect(clippy::too_many_arguments, reason = "one download's every input, said once")]
    pub fn stream_watched(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        bounds: Bounds,
        sink: Sink<'_>,
        what: &str,
        watch: Watch<'_>,
    ) -> anyhow::Result<Response> {
        self.runtime.block_on(self.client.ranged(url, headers, from, bounds, sink, what, watch))
    }
}

impl HyperHttp {
    /// `url`'s body from byte `from`, collected, as a task on this client's runtime for a caller
    /// in a runtime of its own: [`HyperHttp::stream_watched`]'s ranges, progress (said in
    /// blaze-fetch's downloads as `what`) and stall bound. The answer's body is the bytes from
    /// `from` on, whether the server kept to the range (206) or sent the whole (200).
    pub fn get_from_async(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        what: &str,
    ) -> Pending<anyhow::Result<Response>> {
        let (client, in_flight) = (self.client.clone(), self.in_flight.clone());
        let (url, what) = (url.to_owned(), what.to_owned()); // alloc: moved into the task
        let headers: Vec<(String, String)> =
            headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect(); // alloc: as above
        let task = self.runtime.spawn(async move {
            let _turn = in_flight.acquire_owned().await?;
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect(); // alloc: borrowed views of the above
            // The body kept, and the offset of its first byte: `from`, or 0 once the server
            // sends the whole.
            let (mut body, mut base) = (Vec::new(), from); // alloc: the body, for the caller
            let mut sink = |offset: u64, bytes: &[u8]| -> std::io::Result<()> {
                if offset != base + body.len() as u64 {
                    if offset != 0 {
                        return Err(std::io::Error::other(format!(
                            "a piece at byte {offset} came after byte {}",
                            base + body.len() as u64
                        )));
                    }
                    (body, base) = (Vec::new(), 0); // alloc: as above, begun again
                }
                body.extend_from_slice(bytes);
                Ok(())
            };
            let mut answer = client
                .ranged(&url, &headers, from, Bounds::NONE, &mut sink, &what, &mut |_| {})
                .await?;
            if matches!(answer.status, 200 | 206) {
                let skip = usize::try_from(from.saturating_sub(base)).unwrap_or(usize::MAX);
                answer.body = body.split_off(skip.min(body.len()));
            }
            Ok(answer)
        });
        Box::pin(async move { task.await.context("a request's task ended")? })
    }
}

/// What a download is called where it is shown: the last part of its address's path.
fn named(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.trim_end_matches('/').rsplit('/').next().filter(|it| !it.is_empty()).unwrap_or(path)
}

impl Requests {
    async fn fetch(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
        bounds: Bounds,
    ) -> anyhow::Result<Response> {
        let request = Request {
            method,
            // alloc: one per request, handed to the client.
            url: url.to_owned(),
            // alloc: one per request, its headers handed to the client.
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body,
            // Asked for as gzip, decoded by the client and held to `most` once decoded. A
            // streamed body is [`Requests::ranged`]'s, asked for as it is kept.
            gzip: true,
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
        let room = usize::try_from(bounds.most).unwrap_or(usize::MAX);
        // alloc: one per answer, its body for the caller.
        let mut bytes = Vec::new();
        while let Some(piece) = next_piece(&mut answer, &said).await? {
            if piece.len() > room.saturating_sub(bytes.len()) {
                // The client holds the body to `most` itself; past it is its bug, said here.
                let refused =
                    Refused::TooBig { url: said.clone(), most: bounds.most, said: None, decoded: true };
                return Err(refused.into());
            }
            bytes.extend_from_slice(&piece);
        }
        Ok(Response { status, headers, body: bytes })
    }
}

/// The length of one ranged request of a body ([`HyperHttp::stream_watched`]), and how many
/// are in flight at once. GitHub's release CDN holds one connection to well under a megabyte a
/// second from some places, and gives each connection its own share, so a large asset comes
/// as many ranges at once; the first range's answer says the whole length, and a body shorter
/// than one range costs one request, as before.
const PIECE: u64 = 8 << 20;
const PARALLEL: usize = 8;

/// How many times one range is asked for before the download gives up, each from where the
/// last stopped.
const PIECE_TRIES: u32 = 4;

/// How often a streamed download's driver wakes while it waits for bytes: to tell its watcher
/// the line and to check the stall bound.
const TICK: Duration = blaze_fetch::progress::TERMINAL_EVERY;

/// The head of an answer to a ranged GET, the body still to read from `answer`.
struct Opened {
    said: String,
    status: u16,
    headers: Vec<(String, String)>,
    answer: Answer,
}

impl Opened {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(it, _)| it == name).map(|(_, it)| it.as_str())
    }
}

/// `bytes <start>-<end>/<total>` of a 206, or `bytes */<total>` of a 416.
fn content_range(value: &str) -> Option<(Option<(u64, u64)>, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let total = total.trim().parse().ok()?;
    if range.trim() == "*" {
        return Some((None, total));
    }
    let (start, end) = range.split_once('-')?;
    Some((Some((start.trim().parse().ok()?, end.trim().parse().ok()?)), total))
}

impl Requests {
    /// A GET of `url` asking for bytes `from..=to` (`to` open when `None`), not encoded, held to
    /// `bounds`; with `if_range` (the first answer's `ETag`), a body that changed meanwhile comes
    /// whole as a 200, which the caller refuses.
    async fn open(
        &self,
        url: &str,
        headers: &[(String, String)],
        (from, to): (u64, Option<u64>),
        if_range: Option<&str>,
        bounds: Bounds,
    ) -> anyhow::Result<Opened> {
        // alloc: one per request, its headers handed to the client.
        let mut asked: Vec<(String, String)> = headers.to_vec();
        let range = match to {
            Some(to) => format!("bytes={from}-{to}"),
            None => format!("bytes={from}-"),
        };
        asked.push(("range".to_owned(), range));
        if let Some(tag) = if_range {
            asked.push(("if-range".to_owned(), tag.to_owned()));
        }
        let request = Request {
            method: Method::GET,
            url: url.to_owned(), // alloc: one per request
            headers: asked,
            body: Vec::new(),
            gzip: false,
            most: bounds.most,
            https_only: bounds.https_only,
        };
        let mut answer = self.driver.ask(request);
        match next(&mut answer, url).await? {
            Event::Head { url, status, headers, .. } => {
                Ok(Opened { said: redact(&url), status, headers, answer })
            }
            Event::Failed { error, .. } => Err(failed(error)),
            Event::Body { .. } | Event::Done { .. } => {
                anyhow::bail!("{}: blaze-http's client told of a body before its head", redact(url))
            }
        }
    }

    /// One range `from..=to` of `url`, collected, asked again from where it stopped when the
    /// transfer is lost, at most [`PIECE_TRIES`] times. A 200 (the body changed since the first
    /// answer, as `if_range` says) or a range other than asked is refused.
    async fn piece(
        self,
        url: String,
        headers: Vec<(String, String)>,
        (from, to): (u64, u64),
        if_range: Option<String>,
        bounds: Bounds,
        counter: blaze_fetch::downloads::Counter<'static>,
    ) -> anyhow::Result<Vec<u8>> {
        let len = usize::try_from(to - from + 1).unwrap_or(usize::MAX);
        // alloc: one range of a download, at most `PIECE` bytes
        let mut got = Vec::with_capacity(len);
        let mut tries = 0;
        loop {
            let at = from + got.len() as u64;
            let tried: anyhow::Result<()> = async {
                let mut opened =
                    self.open(&url, &headers, (at, Some(to)), if_range.as_deref(), bounds).await?;
                let said = opened.header("content-range").and_then(content_range);
                if opened.status != 206 || said.and_then(|it| it.0) != Some((at, to)) {
                    let error = anyhow::anyhow!(
                        "{} answered a request for bytes {at}-{to} with {} {}: its body changed \
                         since the download began, or it does not keep to the ranges it offers",
                        opened.said,
                        opened.status,
                        opened.header("content-range").unwrap_or("and no Content-Range"),
                    );
                    return Err(error);
                }
                while let Some(bytes) = next_piece(&mut opened.answer, &opened.said).await? {
                    let room = len - got.len();
                    let bytes = &bytes[..bytes.len().min(room)];
                    got.extend_from_slice(bytes);
                    counter.got(bytes.len() as u64);
                }
                Ok(())
            }
            .await;
            match tried {
                Ok(()) if got.len() == len => return Ok(got),
                Ok(()) => {}
                Err(error) if error.downcast_ref::<Transport>().is_none() => return Err(error),
                Err(error) if tries + 1 >= PIECE_TRIES => {
                    return Err(error.context(format!(
                        "bytes {from}-{to} of {}: given up after {PIECE_TRIES} tries",
                        redact(&url)
                    )));
                }
                Err(_) => {}
            }
            tries += 1;
            if tries >= PIECE_TRIES {
                anyhow::bail!(
                    "bytes {from}-{to} of {} ended short {PIECE_TRIES} times, at {} of {len} bytes",
                    redact(&url),
                    got.len()
                );
            }
        }
    }

    /// `url`'s body from byte `from`, into `sink` in order, each piece with its offset in the
    /// whole body: one ranged request for the first [`PIECE`] (whose answer says the length),
    /// then the rest as [`PARALLEL`] ranges at once. A server that ignores ranges answers a 200
    /// of the whole body, which goes to `sink` from offset 0. Registered in blaze-fetch's
    /// downloads as `what` for the status line and the stall bound, `watch`ed every [`TICK`],
    /// and stopped as [`Stalled`] when no byte comes for the stall bound.
    #[expect(clippy::too_many_arguments, reason = "one download's every input, said once")]
    async fn ranged(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        bounds: Bounds,
        sink: Sink<'_>,
        what: &str,
        watch: Watch<'_>,
    ) -> anyhow::Result<Response> {
        let download = blaze_fetch::downloads::Downloads::process().begin(what, url, None);
        // alloc: one per download, its headers for every range.
        let owned: Vec<(String, String)> =
            headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        let first_to = from.saturating_add(PIECE - 1);
        let ticked = |download: &blaze_fetch::downloads::Download<'_>,
                      watch: &mut (dyn FnMut(&blaze_fetch::progress::Line) + Send),
                      said: &str|
         -> anyhow::Result<()> {
            let line = download.line();
            watch(&line);
            if line.idle >= blaze_fetch::progress::STALL {
                return Err(Stalled { url: redact(said), idle: line.idle, line }.into());
            }
            Ok(())
        };
        let mut opened = tick_until(
            self.open(url, &owned, (from, Some(first_to)), None, bounds),
            || ticked(&download, watch, url),
        )
        .await?;
        let length = opened.header("content-length").and_then(|it| it.trim().parse::<u64>().ok());
        let (at, total) = match opened.status {
            200 => (0, length),
            206 => match opened.header("content-range").and_then(content_range) {
                Some((Some((start, _)), total)) if start == from => (from, Some(total)),
                _ => anyhow::bail!(
                    "{} answered a request for bytes {from}- with 206 and Content-Range {:?}, \
                     which is not from byte {from}",
                    opened.said,
                    opened.header("content-range")
                ),
            },
            // All of it is here already: a resumed download that had every byte.
            416 if from > 0
                && opened.header("content-range").and_then(content_range)
                    == Some((None, from)) =>
            {
                download.resumed(from);
                download.set_total(Some(from));
                watch(&download.line());
                return Ok(Response { status: 206, headers: opened.headers, body: Vec::new() });
            }
            status => {
                // An error's body, for its message: its first `ERROR_BODY_MOST` bytes.
                let room = usize::try_from(ERROR_BODY_MOST).unwrap_or(usize::MAX);
                // alloc: one per error answer
                let mut body = Vec::new();
                while let Some(piece) = next_piece(&mut opened.answer, &opened.said).await? {
                    body.extend_from_slice(&piece[..piece.len().min(room - body.len())]);
                    if body.len() >= room {
                        break;
                    }
                }
                return Ok(Response { status, headers: opened.headers, body });
            }
        };
        if let Some(total) = total.filter(|it| *it > bounds.most) {
            let refused = Refused::TooBig { url: opened.said, most: bounds.most, said: Some(total), decoded: false };
            return Err(refused.into());
        }
        download.resumed(at);
        download.set_total(total);
        let status = opened.status;
        let said = opened.said.clone();
        let tag = opened.header("etag").map(str::to_owned);
        let head = opened.headers.clone(); // clone: the answer's head, for the caller
        // The first answer's body: the whole of a 200, or the first range.
        let mut offset = at;
        loop {
            let piece =
                tick_until(next_piece(&mut opened.answer, &said), || ticked(&download, watch, &said))
                    .await?;
            let Some(piece) = piece else { break };
            if offset.saturating_add(piece.len() as u64) > bounds.most {
                let refused = Refused::TooBig { url: said, most: bounds.most, said: None, decoded: false };
                return Err(refused.into());
            }
            sink(offset, &piece).with_context(|| format!("keeping the body of {said}"))?;
            offset += piece.len() as u64;
            download.got(piece.len() as u64);
        }
        drop(opened);
        let total = match (status, total) {
            (206, Some(total)) => total,
            _ => {
                watch(&download.line());
                return Ok(Response { status, headers: head, body: Vec::new() });
            }
        };
        // The rest, `PARALLEL` ranges in flight, handed on in order: memory holds at most
        // `PARALLEL` ranges.
        let mut next_from = offset;
        let mut flying = std::collections::VecDeque::new(); // alloc: at most `PARALLEL` ranges in flight
        let spawn = |start: u64| {
            let to = start.saturating_add(PIECE - 1).min(total - 1);
            let task = tokio::spawn(self.clone().piece(
                url.to_owned(),
                owned.clone(), // clone: each range asks with the download's headers
                (start, to),
                tag.clone(), // clone: as above
                bounds,
                download.counter(),
            ));
            (start, to, task)
        };
        while next_from < total || !flying.is_empty() {
            while flying.len() < PARALLEL && next_from < total {
                let (start, to, task) = spawn(next_from);
                next_from = to + 1;
                flying.push_back((start, task));
            }
            let Some((start, mut task)) = flying.pop_front() else { break };
            let joined = tick_until(async { (&mut task).await.map_err(anyhow::Error::from) }, || {
                ticked(&download, watch, &said)
            })
            .await;
            let bytes = match joined.and_then(|it| it) {
                Ok(bytes) => bytes,
                Err(error) => {
                    task.abort();
                    for (_, other) in flying {
                        other.abort();
                    }
                    return Err(error);
                }
            };
            debug_assert_eq!(start, offset);
            sink(offset, &bytes).with_context(|| format!("keeping the body of {said}"))?;
            offset += bytes.len() as u64;
        }
        watch(&download.line());
        Ok(Response { status: 206, headers: head, body: Vec::new() })
    }
}

/// `work`, with `tick` run every [`TICK`] while it is not done; an error of `tick` (the stall
/// bound) ends the wait with it.
async fn tick_until<T>(
    work: impl Future<Output = anyhow::Result<T>>,
    mut tick: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<T> {
    let mut work = std::pin::pin!(work);
    loop {
        match tokio::time::timeout(TICK, &mut work).await {
            Ok(done) => return done,
            Err(_) => tick()?,
        }
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
async fn next_piece(answer: &mut Answer, said: &str) -> anyhow::Result<Option<Vec<u8>>> {
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
        Kind::TooBig { most, said: length, decoded } => {
            Refused::TooBig { url: said, most, said: length, decoded }.into()
        }
        Kind::NoAnswer { after, .. } => {
            Transport(format!("no answer from {said} in {after:?}")).into()
        }
        Kind::Stalled { after, .. } => {
            Transport(format!("transfer from {said} stalled for {after:?}")).into()
        }
        Kind::Io { step, host, port, why } => {
            Transport(scrub(&format!("{step} {host}:{port} for {said}: {why}"))).into()
        }
        Kind::Gzip(why) => Transport(format!("{said} sent gzip that is not: {why}")).into(),
        kind @ (Kind::Url(_)
        | Kind::Header(_)
        | Kind::Redirects { .. }
        | Kind::RedirectsNowhere { .. }
        | Kind::InUse) => {
            anyhow::anyhow!("{said}: {kind}")
        }
    }
}

impl Http for HyperHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        self.send(Method::GET, url, headers, Vec::new())
    }

    fn stream(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        sink: Sink<'_>,
    ) -> anyhow::Result<Response> {
        self.stream_watched(url, headers, from, Bounds::NONE, sink, named(url), &mut |_| {})
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
            client.fetch(Method::GET, &url, &headers, Vec::new(), Bounds::NONE).await
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
        let to =
            format!("http://127.0.0.1:{port}/b?X-Amz-Credential=me@example&X-Amz-Signature=secret");
        let answers = [
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
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
        let got = http.stream(&url, &[], 0, &mut |_, _| Ok(())).unwrap_err();
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
                        if stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        };
        std::thread::Builder::new()
            .name("a keep-alive server".to_owned())
            .spawn(move || {
                server.block_on(async move {
                    _ = tokio::time::timeout(Duration::from_secs(60), serving).await
                })
            })
            .unwrap();

        let http = HyperHttp::new(Duration::from_secs(10)).unwrap();
        let url = format!("http://127.0.0.1:{port}/a");
        for _ in 0..3 {
            let got = http.get(&url, &[]).unwrap();
            assert_eq!((got.status, &got.body[..]), (200, &b"ok"[..]));
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "three requests should have shared one connection"
        );

        let refusing = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = refusing.local_addr().unwrap().port();
        drop(refusing);
        let got = http.get(&format!("http://127.0.0.1:{closed}/a"), &[]).unwrap_err();
        let said = format!("{got:#}");
        assert!(got.downcast_ref::<Transport>().is_some(), "{said}");
        assert!(
            said.contains(&format!(
                "connecting to 127.0.0.1:{closed} for http://127.0.0.1:{closed}/a"
            )),
            "{said}"
        );
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
                (
                    "https://github.com/o/r/releases/download/v1/a.tar.gz",
                    "http://objects.example/a"
                )
            ),
            other => panic!("{other:?}"),
        }
    }
}
