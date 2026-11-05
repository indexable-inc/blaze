//! Asking a server for a file: the [`Http`] trait the index and the downloads go through, the
//! retries of cargo's `net.retry` (`src/util/network/retry.rs`), and [`HyperHttp`], over
//! blaze-http (blaze's sans-IO HTTP/1.1 client, rustls with ring, driven on tokio); without the
//! `net` feature (the seed profile) a [`HyperHttp`] whose every request is [`NoNet`].

use std::{
    fmt,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};

#[cfg(feature = "net")]
mod client;
#[cfg(not(feature = "net"))]
mod offline;

/// Whether this blaze can ask a server for anything: built with the `net` feature. Without it
/// every request is [`NoNet`], so what is not in the store or the sources already can't be had,
/// and a build says so for every such package at once.
pub const NET: bool = cfg!(feature = "net");

#[cfg(feature = "net")]
pub use client::HyperHttp;
#[cfg(not(feature = "net"))]
pub use offline::{HyperHttp, NoNet};

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

/// What a streamed download hands its watcher, every [`TICK`] and once at its end.
// dyn: the caller's printer, once a tick
pub type Watch<'w> = &'w mut (dyn FnMut(&blaze_fetch::progress::Line) + Send);

/// A download that got no byte for [`blaze_fetch::progress::STALL`]: stopped, not tried again
/// (what it got is the caller's to resume from).
#[derive(Debug)]
pub struct Stalled {
    pub url: String,
    pub idle: Duration,
    pub line: blaze_fetch::progress::Line,
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no data from {} for {}, with {} come: the download is stopped as stalled \
             (a download may go {} without a byte)",
            self.url,
            blaze_fetch::progress::Secs(self.idle),
            blaze_fetch::progress::Bytes(self.line.done),
            blaze_fetch::progress::Secs(blaze_fetch::progress::STALL),
        )
    }
}

impl std::error::Error for Stalled {}

/// Work started at once and finished on whichever thread does it, awaited from any executor.
// dyn: what `dyn Http` returns, one per network request, which costs far more than its box.
pub type Pending<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Where a streamed body goes, a piece at a time with its offset in the whole body
/// ([`Http::stream`]); an error stops the transfer.
pub type Sink<'s> = &'s mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send);

/// What asks servers for files. An error is one where no answer came, which is worth trying
/// again, as cargo tries a connection or a transfer that failed again.
pub trait Http: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response>;

    /// [`Http::get`], sent before this returns and answered when it is done, so that many
    /// requests are in flight at once. By default it is asked and answered on this thread.
    fn get_async(&self, url: &str, headers: &[(&str, &str)]) -> Pending<anyhow::Result<Response>> {
        Box::pin(std::future::ready(self.get(url, headers)))
    }

    /// [`Http::get`] of the body from byte `from`, which goes to `sink` as it comes, each piece
    /// with its offset in the whole body, instead of into the answer, whose `body` is then
    /// empty: a 206 from `from`, or a 200 of the whole body from 0 (a server that ignores
    /// ranges). Any other answer is given for its error, with at most the first 64 KiB of its
    /// body (HyperHttp's bound: no error page fills memory). HyperHttp asks a large body as
    /// many ranges at once and shows it among blaze-fetch's downloads. By default (a test's
    /// fake) the whole body is got and handed over from 0.
    fn stream(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        _from: u64,
        sink: Sink<'_>,
    ) -> anyhow::Result<Response> {
        let mut answer = self.get(url, headers)?;
        if answer.status == 200 {
            let mut at = 0u64;
            for piece in answer.body.chunks(64 * 1024) {
                let offset = at;
                at += piece.len() as u64;
                sink(offset, piece).map_err(|e| {
                    anyhow::anyhow!("keeping the body of {}: {e}", blaze_fetch::redact(url))
                })?;
            }
            answer.body = Vec::new(); // alloc: empty, allocates nothing
        }
        Ok(answer)
    }

    /// A wait that holds no thread; by default it holds this one ([`wait_out`]).
    fn sleep(&self, duration: Duration) -> Pending<()> {
        wait_out(duration);
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

    /// Whether another try may get a better answer: a server error, or too many requests.
    pub fn is_spurious(&self) -> bool {
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
                Err(e) => match self.again(&mut tried, &e, SystemTime::now()) {
                    Some(wait) => wait_out(wait),
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
        // dyn: the I/O driver, asked only to sleep between tries of a failed request.
        http: &dyn Http,
        mut attempt: impl FnMut() -> F,
    ) -> anyhow::Result<T> {
        let mut tried = 0;
        loop {
            match attempt().await {
                Err(e) => match self.again(&mut tried, &e, SystemTime::now()) {
                    Some(wait) => http.sleep(wait).await,
                    None => return Err(e),
                },
                it => return it,
            }
        }
    }

    /// How long to wait before trying again after `e`, said as cargo says it, at `now` (the
    /// first wait's jitter is read from it); none when `e` is not worth another try or the
    /// tries are used up.
    fn again(&self, tried: &mut u32, e: &anyhow::Error, now: SystemTime) -> Option<Duration> {
        if *tried >= self.retries || !spurious(e) {
            return None;
        }
        let left = self.retries - *tried;
        if !self.quiet {
            let tries = if left == 1 { "try" } else { "tries" };
            eprintln!(
                "warning: spurious network error ({left} {tries} remaining): {}",
                blaze_fetch::scrub(&format!("{e:#}"))
            );
        }
        *tried += 1;
        let after = e.downcast_ref::<HttpNotSuccessful>().and_then(retry_after);
        let sleep = after.map_or_else(
            || self.fixed_sleep_ms.unwrap_or_else(|| sleep_ms(*tried, now)),
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

/// The wait before try `tried + 1`: half a second and up to a second more at random first
/// (from `now`'s milliseconds), then three seconds more each time, to ten.
fn sleep_ms(tried: u32, now: SystemTime) -> u64 {
    if tried == 1 {
        let since = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        INITIAL_RETRY_SLEEP_BASE_MS + u64::from(since.subsec_millis()) % INITIAL_RETRY_JITTER_MS
    } else {
        (u64::from(tried - 1) * 3 * 1000 + INITIAL_RETRY_SLEEP_BASE_MS).min(MAX_RETRY_SLEEP_MS)
    }
}

/// Holds this thread for a wait [`Retry::again`] gave: the retries' one timer, where the asker
/// has no runtime to wait on.
#[allow(
    clippy::disallowed_methods,
    reason = "the retries' timer: `Retry::again` decides the wait, from the time it is given"
)]
fn wait_out(wait: Duration) {
    std::thread::sleep(wait);
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

/// What one request ([`HyperHttp::get_bounded`]) is held to, beyond what every request is.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    /// The most bytes its body may be, as sent and once decoded: a body that would be more
    /// stops being read there ([`Refused::TooBig`]), so a server can't fill memory first.
    pub most: u64,
    /// Whether it is asked, and its redirects followed, over HTTPS only: a plain `http://`
    /// address, given or redirected to, is [`Refused::NotHttps`] or [`Refused::Downgraded`].
    pub https_only: bool,
}

impl Bounds {
    /// What every request is held to: no limit of its own, and plain HTTP where it is asked.
    pub const NONE: Bounds = Bounds { most: u64::MAX, https_only: false };
}

/// A request refused by its [`Bounds`]: an answer that is not worth asking again for.
#[derive(Debug)]
pub enum Refused {
    /// `url`, asked over HTTPS only, is no `https://` address.
    NotHttps { url: String },
    /// `from`, asked over HTTPS only, redirects to `to`, which is not `https://`.
    Downgraded { from: String, to: String },
    /// `url`'s body is more than `most` bytes: as its `Content-Length` says (`said`), else as
    /// read (`decoded`: once its gzip was decoded), where reading stopped.
    TooBig { url: String, most: u64, said: Option<u64>, decoded: bool },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotHttps { url } => {
                write!(f, "{url} is not an https:// address, and this is fetched over HTTPS only")
            }
            Refused::Downgraded { from, to } => write!(
                f,
                "{from} redirects to {to}, which is not https://, and this is fetched over HTTPS only"
            ),
            Refused::TooBig { url, most, said: Some(said), .. } => write!(
                f,
                "{url} is {said} bytes as its Content-Length says, more than the {most} it may be"
            ),
            Refused::TooBig { url, most, said: None, decoded } => write!(
                f,
                "{url} is more than the {most} bytes it may be{}, and reading it stopped there",
                if *decoded { " once its gzip is decoded" } else { "" }
            ),
        }
    }
}

impl std::error::Error for Refused {}

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
        let at = |nanos| SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, nanos);
        assert_eq!(sleep_ms(1, at(0)), 500);
        assert_eq!(sleep_ms(1, at(250_000_000)), 750);
        assert_eq!(sleep_ms(1, at(999_999_999)), 1499);
        assert_eq!(sleep_ms(2, at(250_000_000)), 3500);
        assert_eq!(sleep_ms(3, at(0)), 6500);
        assert_eq!(sleep_ms(4, at(0)), 9500);
        assert_eq!(sleep_ms(5, at(0)), 10000);
        let answer = |code, after: &str| HttpNotSuccessful {
            url: String::new(),
            code,
            retry_after: Some(after.to_owned()),
            body: String::new(),
        };
        assert_eq!(retry_after(&answer(429, "2")), Some(2000));
        assert_eq!(retry_after(&answer(500, "2")), None);
    }

    /// The waits [`Retry::again`] gives, at a time the test picks: a `Retry-After` of seconds
    /// on a 429 or a 503, to ten seconds; cargo's schedule where there is none, it is not
    /// seconds, or the answer is another; none past the tries or for an answer not worth one.
    #[test]
    fn the_waits_between_tries_follow_retry_after_then_cargos_schedule() {
        let now = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 250_000_000);
        let retry = Retry { retries: 3, quiet: true, fixed_sleep_ms: None };
        let failed = |status: u16, after: Option<&str>| -> anyhow::Error {
            let headers =
                after.map(|it| vec![("retry-after".to_owned(), it.to_owned())]).unwrap_or_default();
            HttpNotSuccessful::new("u", &Response { status, headers, body: Vec::new() }).into()
        };
        let waits = |errors: &[anyhow::Error]| -> Vec<Option<u64>> {
            let mut tried = 0;
            errors
                .iter()
                .map(|e| {
                    retry.again(&mut tried, e, now).map(|it| u64::try_from(it.as_millis()).unwrap())
                })
                .collect()
        };
        assert_eq!(
            waits(&[failed(429, Some("2")), failed(503, Some("4")), failed(429, Some("60"))]),
            [Some(2000), Some(4000), Some(10000)]
        );
        assert_eq!(
            waits(&[failed(500, Some("2")), failed(503, None), failed(429, Some("soon"))]),
            [Some(750), Some(3500), Some(6500)]
        );
        assert_eq!(
            waits(&[failed(500, None), failed(500, None), failed(500, None), failed(500, None)]),
            [Some(750), Some(3500), Some(6500), None]
        );
        assert_eq!(waits(&[failed(403, Some("2"))]), [None]);
        let fixed = Retry { fixed_sleep_ms: Some(7), ..retry };
        let mut tried = 0;
        assert_eq!(
            fixed.again(&mut tried, &failed(429, Some("2")), now),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            fixed.again(&mut tried, &failed(500, None), now),
            Some(Duration::from_millis(7))
        );
    }
}
