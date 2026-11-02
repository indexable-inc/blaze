//! The GitHub App: its JWT, its installation token and the check runs, over TLS verified
//! against the machine's roots.
//!
//! No token, JWT or key is ever in an error, a log or a `Debug`.

use std::{
    borrow::Cow,
    fmt,
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use blaze_api::http::{self, HttpResponse};
use parking_lot::Mutex;
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};
use serde::Deserialize;

use crate::{CheckRun, CheckStatus};

/// GitHub's cap on a check run's summary, in characters.
pub const MOST_SUMMARY: usize = 65535;

const CUT: &str = "\n\n(cut here: the summary was longer than GitHub's 65535 characters)";

/// How long a connect, a read or a write may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes of a GitHub answer read.
const MOST_ANSWER: usize = 8 << 20;

/// A connection idle longer than this is not reused: the server may be closing it.
const KEEP_IDLE: Duration = Duration::from_secs(20);

/// A token is minted again this long before it expires.
const RENEW_BEFORE: u64 = 5 * 60;

/// The App's private key; Debug and Display never show it.
pub struct Key(RsaKeyPair);

impl Key {
    /// "RSA PRIVATE KEY" (PKCS#1, what GitHub issues) or "PRIVATE KEY" (PKCS#8).
    pub fn from_pem(pem: &str) -> Result<Key, GitHubError> {
        let mut lines = pem.lines().map(str::trim).skip_while(|line| line.is_empty());
        let begin = lines.next().unwrap_or_default();
        let label = begin
            .strip_prefix("-----BEGIN ")
            .and_then(|it| it.strip_suffix("-----"))
            .ok_or_else(|| GitHubError::new("the App's key is not PEM: no -----BEGIN line first"))?;
        let end = format!("-----END {label}-----"); // alloc: once, at start, the App's key
        let mut base64 = String::new(); // alloc: once, at start, the App's key
        let mut ended = false;
        for line in lines.by_ref() {
            if line == end {
                ended = true;
                break;
            }
            if line.contains(':') {
                return Err(GitHubError::new(
                    "the App's key PEM has headers, as an encrypted key does; give it unencrypted",
                ));
            }
            base64.push_str(line);
        }
        if !ended {
            // alloc: error path
            return Err(GitHubError::new(format!("the App's key PEM has no {end} line")));
        }
        if lines.any(|line| !line.is_empty()) {
            return Err(GitHubError::new("the App's key PEM has more after its end line"));
        }
        let der = STANDARD
            .decode(base64.as_bytes())
            // alloc: error path
            .map_err(|_| GitHubError::new(format!("the App's key PEM's {label} is not base64")))?;
        let pair = match label {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
            _ => {
                return Err(GitHubError::new(format!( // alloc: error path
                    "the App's key PEM is a {label}, not an RSA PRIVATE KEY or a PRIVATE KEY"
                )));
            }
        };
        pair.map(Key)
            // alloc: error path
            .map_err(|error| GitHubError::new(format!("the App's {label} was refused: {error}")))
    }

    /// The JWT the App authenticates as at `now` (Unix seconds): RS256, issued a minute back
    /// for clock drift, expiring in nine minutes (GitHub's limit is ten).
    fn jwt(&self, app: u64, now: u64) -> Result<String, GitHubError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        // alloc: once per token minted, about once an hour
        let claims = format!(r#"{{"iat":{},"exp":{},"iss":{app}}}"#, now.saturating_sub(60), now + 540);
        // alloc: once per token minted, about once an hour
        let mut jwt = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims));
        // alloc: once per token minted, about once an hour
        let mut signature = vec![0; self.0.public().modulus_len()];
        self.0
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), jwt.as_bytes(), &mut signature)
            .map_err(|_| GitHubError::new("signing the App's JWT failed"))?;
        jwt.push('.');
        jwt.push_str(&URL_SAFE_NO_PAD.encode(signature));
        Ok(jwt)
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(hidden)")
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the App's private key")
    }
}

#[derive(Debug)]
pub struct App {
    pub id: u64,
    pub installation: u64,
    pub key: Key,
    /// The API's host, "api.github.com".
    pub api: String,
}

/// An installation token; Debug never shows it.
// clone: once per token per lane, out of the shared cache
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(token: String) -> Token {
        Token(token)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct GitHubError {
    pub what: String,
    /// GitHub's HTTP status, when it answered.
    pub status: Option<u16>,
}

impl GitHubError {
    fn new(what: impl Into<String>) -> GitHubError {
        GitHubError { what: what.into(), status: None }
    }
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (GitHub answered {status})", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for GitHubError {}

/// What the service needs of GitHub, so it can be tested with a fake.
pub trait Checks {
    /// Creates the check run on its commit; returns its id.
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError>;
    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError>;
}

/// A token and when it expires, in Unix seconds.
type Minted = Option<(Token, u64)>;

/// The App, installed on one repository: one connection. [`GitHub::lane`] makes another that
/// shares the App and its token, so check runs post on several connections at once.
pub struct GitHub {
    app: Arc<App>,
    repo: String,
    tls: Arc<ClientConfig>,
    /// This connection's copy of the token.
    token: Minted,
    /// The token every lane shares: one mints it and the others copy it, once an hour.
    minted: Arc<Mutex<Minted>>,
    kept: Option<Kept>,
    /// The next request's JSON body, then the whole request; both reused, so a check run's
    /// update allocates no buffer once they have grown to its size.
    body: Vec<u8>,
    request: Vec<u8>,
}

/// What a request is to.
#[derive(Clone, Copy)]
enum Route {
    /// `/app/installations/<installation>/access_tokens`
    Mint,
    /// `/repos/<repo>/check-runs`
    CheckRuns,
    /// `/repos/<repo>/check-runs/<id>`
    CheckRun(u64),
}

/// A route's path, written where it goes with no string of its own.
struct PathOf<'a> {
    route: Route,
    installation: u64,
    repo: &'a str,
}

impl fmt::Display for PathOf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.route {
            Route::Mint => write!(f, "/app/installations/{}/access_tokens", self.installation),
            Route::CheckRuns => write!(f, "/repos/{}/check-runs", self.repo),
            Route::CheckRun(id) => write!(f, "/repos/{}/check-runs/{id}", self.repo),
        }
    }
}

/// How a request authenticates: as the App, with its JWT, or as the installation, with the
/// token [`GitHub::fresh`] keeps.
#[derive(Clone, Copy)]
enum Auth<'a> {
    Bearer(&'a str),
    Installation,
}

/// The connection of the last answer, kept for the next request.
struct Kept {
    stream: StreamOwned<ClientConnection, TcpStream>,
    idle_since: Instant,
}

/// Why an exchange failed. `stale`: nothing came back, as when the server closed the
/// connection while it was idle.
struct Failed {
    what: String,
    stale: bool,
}

fn exchange(
    stream: &mut StreamOwned<ClientConnection, TcpStream>,
    request: &[u8],
) -> Result<HttpResponse, Failed> {
    if let Err(error) = stream.write_all(request).and_then(|()| stream.flush()) {
        return Err(Failed { what: format!("sending: {error}"), stale: true }); // alloc: error path
    }
    let mut counted = Counted { from: stream, read: 0 };
    http::read_response(&mut counted, MOST_ANSWER).map_err(|error| Failed {
        what: format!("reading the answer: {error}"), // alloc: error path
        stale: counted.read == 0,
    })
}

/// A reader that counts what it gave.
struct Counted<'a, R> {
    from: &'a mut R,
    read: usize,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.from.read(buffer)?;
        self.read += read;
        Ok(read)
    }
}

impl GitHub {
    /// `repo` is "owner/name". The machine's roots are loaded here: none is an error.
    pub fn new(app: App, repo: String) -> Result<GitHub, GitHubError> {
        let part = |it: &str| {
            !it.is_empty()
                && it != "."
                && it != ".."
                && it.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        };
        if !matches!(repo.split_once('/'), Some((owner, name)) if part(owner) && part(name)) {
            // alloc: error path
            return Err(GitHubError::new(format!("{repo:?} is not a repository's owner/name")));
        }
        if app.api.is_empty()
            || !app.api.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte))
        {
            // alloc: error path
            return Err(GitHubError::new(format!("{:?} is not a host name for GitHub's API", app.api)));
        }
        // alloc: none until the first call, then reused
        let (body, request) = (Vec::new(), Vec::new());
        let minted = Arc::new(Mutex::new(None)); // alloc: once per App, shared by its lanes
        let app = Arc::new(app); // alloc: once per App, shared by its lanes
        Ok(GitHub { app, repo, tls: tls()?, token: None, minted, kept: None, body, request })
    }

    /// Another connection to GitHub for the same App and repository, sharing its token.
    pub fn lane(&self) -> GitHub {
        GitHub {
            app: Arc::clone(&self.app),
            repo: self.repo.clone(), // clone: once per lane, at start
            tls: Arc::clone(&self.tls),
            token: None,
            minted: Arc::clone(&self.minted),
            kept: None,
            body: Vec::new(), // alloc: none until the lane's first call, then reused
            request: Vec::new(), // alloc: none until the lane's first call, then reused
        }
    }

    /// The installation token, minted (and kept until five minutes before it expires).
    pub fn token(&mut self) -> Result<Token, GitHubError> {
        self.fresh().cloned()
    }

    /// The token kept, minted again first when it is within five minutes of expiring. A lane
    /// whose copy is stale takes the shared one when another lane has minted it already; the
    /// lock is taken only then, so once an hour.
    fn fresh(&mut self) -> Result<&Token, GitHubError> {
        let now = now()?;
        let valid = |held: &Minted| matches!(held, Some((_, expires)) if now + RENEW_BEFORE < *expires);
        if !valid(&self.token) {
            self.token = None;
            let minted = Arc::clone(&self.minted);
            let mut shared = minted.lock();
            if !valid(&*shared) {
                *shared = None;
                let jwt = self.app.key.jwt(self.app.id, now)?;
                self.body.clear();
                let answer = self.call("POST", Route::Mint, Auth::Bearer(&jwt), true)?;
                let token: MintedToken<'_> = serde_json::from_slice(&answer.body).map_err(|_| {
                    GitHubError::new("GitHub's answer to minting a token is not a token and its expires_at")
                })?;
                let expires = unix_seconds(&token.expires_at)?;
                *shared = Some((Token(token.token), expires));
            }
            self.token = (*shared).clone(); // clone: once per token per lane
        }
        Ok(&self.token.as_ref().expect("a token, kept or just minted").0)
    }

    /// One request with the body in `self.body`, on the kept connection when it is fresh; a
    /// non-2xx answer is an error with GitHub's `message`. A kept connection the server closed
    /// while idle is replaced and the request sent again only when `replayable`: sending it
    /// twice must change nothing. Nothing is allocated for the request itself: its bytes go into
    /// `self.request`, and the error's text is made only for an error.
    fn call(
        &mut self,
        method: &str,
        route: Route,
        auth: Auth<'_>,
        replayable: bool,
    ) -> Result<HttpResponse, GitHubError> {
        let GitHub { app, repo, tls, token, kept, body, request, .. } = self;
        let tls: &Arc<ClientConfig> = tls;
        let path = PathOf { route, installation: app.installation, repo: repo.as_str() };
        let api = app.api.as_str();
        // alloc: error path, called only for an error's text
        let what = || format!("{method} https://{api}{path}");
        let authorization = match auth {
            Auth::Bearer(jwt) => ("Bearer ", jwt),
            Auth::Installation => match token {
                Some((token, _)) => ("token ", token.expose()),
                // alloc: error path
                None => return Err(GitHubError::new(format!("{}: no installation token", what()))),
            },
        };
        write_request(request, method, api, &path, authorization, body);
        // alloc: error path
        let dial = || connect(api, tls).map_err(|error| GitHubError::new(format!("{}: {error}", what())));
        let held = kept.take().filter(|it| it.idle_since.elapsed() < KEEP_IDLE);
        let reused = held.is_some();
        let mut stream = match held {
            Some(held) => held.stream,
            None => dial()?,
        };
        let mut exchanged = exchange(&mut stream, request);
        if reused && replayable && matches!(exchanged, Err(Failed { stale: true, .. })) {
            stream = dial()?;
            exchanged = exchange(&mut stream, request);
        }
        // alloc: error path
        let answer = exchanged.map_err(|failed| GitHubError::new(format!("{}: {}", what(), failed.what)))?;
        let closes = answer
            .header("Connection")
            .is_some_and(|it| it.split(',').any(|token| token.trim().eq_ignore_ascii_case("close")));
        if !closes {
            *kept = Some(Kept { stream, idle_since: Instant::now() });
        }
        if (200..300).contains(&answer.status) {
            return Ok(answer);
        }
        let message = serde_json::from_slice::<Message>(&answer.body)
            .map_or_else(|_| "no message".to_owned(), |it| it.message); // alloc: error path
        // alloc: error path
        Err(GitHubError { what: format!("{}: {message}", what()), status: Some(answer.status) })
    }
}

/// A TLS connection to `api`'s port 443, with the timeouts set.
fn connect(api: &str, tls: &Arc<ClientConfig>) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
    // alloc: error path
    let addresses = (api, 443).to_socket_addrs().map_err(|error| format!("resolving {api}: {error}"))?;
    let mut last = None;
    let mut connected = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, TIMEOUT) {
            Ok(tcp) => {
                connected = Some(tcp);
                break;
            }
            // alloc: error path
            Err(error) => last = Some(format!("connecting to {address}: {error}")),
        }
    }
    // alloc: error path
    let tcp = connected.ok_or_else(|| last.unwrap_or_else(|| format!("{api} resolved to no address")))?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .and_then(|()| tcp.set_write_timeout(Some(TIMEOUT)))
        .map_err(|error| format!("setting timeouts: {error}"))?; // alloc: error path
    let name =
        // alloc: once per connection, the TLS name rustls keeps
        ServerName::try_from(api.to_owned()).map_err(|error| format!("{api:?} as a TLS server name: {error}"))?;
    // alloc: error path
    let tls = ClientConnection::new(Arc::clone(tls), name).map_err(|error| format!("starting TLS: {error}"))?;
    Ok(StreamOwned::new(tls, tcp))
}

/// The request's bytes into `out`, the same as [`http::request`] makes with these headers:
/// `Host`, `Authorization`, `Accept`, `X-GitHub-Api-Version`, `User-Agent`, `Content-Type` when
/// there is a body, `Content-Length`.
fn write_request(
    out: &mut Vec<u8>,
    method: &str,
    host: &str,
    path: &impl fmt::Display,
    authorization: (&str, &str),
    body: &[u8],
) {
    out.clear();
    let (scheme, credential) = authorization;
    write!(out, "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {scheme}{credential}\r\n")
        .expect("a Vec takes every byte");
    out.extend_from_slice(
        b"Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\nUser-Agent: blaze-ci\r\n",
    );
    if !body.is_empty() {
        out.extend_from_slice(b"Content-Type: application/json\r\n");
    }
    write!(out, "Content-Length: {}\r\n\r\n", body.len()).expect("a Vec takes every byte");
    out.extend_from_slice(body);
}

impl Checks for GitHub {
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError> {
        self.fresh()?;
        check_run(&mut self.body, run, true);
        let answer = self.call("POST", Route::CheckRuns, Auth::Installation, false)?;
        let created: Created = serde_json::from_slice(&answer.body).map_err(|error| {
            // alloc: error path
            GitHubError::new(format!("GitHub's answer to creating check run {}: {error}", run.name))
        })?;
        Ok(created.id)
    }

    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError> {
        self.fresh()?;
        check_run(&mut self.body, run, false);
        self.call("PATCH", Route::CheckRun(id), Auth::Installation, true)?;
        Ok(())
    }
}

/// A check run's JSON into `out`, written straight from `run` (no `serde_json::Value`, no copy
/// of the summary), keys sorted as serde_json's map sorts them. `head_sha` is only taken when
/// one is created.
fn check_run(out: &mut Vec<u8>, run: &CheckRun, create: bool) {
    let (status, conclusion, title) = match run.status {
        CheckStatus::Queued => ("queued", None, "Queued"),
        CheckStatus::InProgress => ("in_progress", None, "Running"),
        CheckStatus::Success => ("completed", Some("success"), "Passed"),
        CheckStatus::Failure => ("completed", Some("failure"), "Failed"),
        CheckStatus::Cancelled => ("completed", Some("cancelled"), "Cancelled"),
    };
    out.clear();
    out.push(b'{');
    if let Some(conclusion) = conclusion {
        out.extend_from_slice(b"\"conclusion\":\"");
        out.extend_from_slice(conclusion.as_bytes());
        out.extend_from_slice(b"\",");
    }
    if create {
        out.extend_from_slice(b"\"head_sha\":");
        json_string(out, &[&run.commit]);
        out.push(b',');
    }
    out.extend_from_slice(b"\"name\":");
    json_string(out, &[&run.name]);
    out.extend_from_slice(b",\"output\":{\"summary\":");
    let (kept, cut) = summary(&run.summary);
    json_string(out, &[kept, cut]);
    out.extend_from_slice(b",\"title\":\"");
    out.extend_from_slice(title.as_bytes());
    out.extend_from_slice(b"\"},\"status\":\"");
    out.extend_from_slice(status.as_bytes());
    out.extend_from_slice(b"\"}");
}

/// `parts`, one after another, as one JSON string escaped byte for byte as serde_json escapes:
/// `"` and `\`, the five controls with a short form, every other control as `\u00xx` in lower
/// case, and nothing else.
fn json_string(out: &mut Vec<u8>, parts: &[&str]) {
    out.push(b'"');
    for part in parts {
        escape(out, part.as_bytes());
    }
    out.push(b'"');
}

const LOW: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// Whether any byte of `word` is a control, `"` or `\`. Each test (Mycroft's has-less-than and
/// has-zero) can't tell which byte, but is exact for whether any is.
fn needs_escape(word: u64) -> bool {
    let less = |n: u64| word.wrapping_sub(LOW * n) & !word & HIGH;
    let zero = |x: u64| x.wrapping_sub(LOW) & !x & HIGH;
    less(0x20) | zero(word ^ (LOW * u64::from(b'"'))) | zero(word ^ (LOW * u64::from(b'\\'))) != 0
}

/// `bytes` escaped onto `out`, eight at a time while none of them needs it, the runs between
/// escapes copied whole.
fn escape(out: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let (mut start, mut at) = (0, 0);
    while at < bytes.len() {
        if let Some(word) = bytes.get(at..at + 8) {
            let word = u64::from_le_bytes(word.try_into().expect("eight bytes"));
            if !needs_escape(word) {
                at += 8;
                continue;
            }
        }
        let byte = bytes[at];
        let short: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0x00..=0x1f => b"",
            _ => {
                at += 1;
                continue;
            }
        };
        out.extend_from_slice(&bytes[start..at]);
        if short.is_empty() {
            let (high, low) = (HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]);
            out.extend_from_slice(&[b'\\', b'u', b'0', b'0', high, low]);
        } else {
            out.extend_from_slice(short);
        }
        at += 1;
        start = at;
    }
    out.extend_from_slice(&bytes[start..]);
}

/// `text` cut to GitHub's limit at a char boundary, and the note that it was cut, empty when it
/// was not. Bytes are counted, never fewer than characters, so the cut text is within the limit
/// however GitHub counts.
fn summary(text: &str) -> (&str, &'static str) {
    if text.len() <= MOST_SUMMARY {
        return (text, "");
    }
    let mut at = MOST_SUMMARY - CUT.len();
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    (&text[..at], CUT)
}

/// A client config with the machine's roots, the ring provider, TLS 1.2 and 1.3.
fn tls() -> Result<Arc<ClientConfig>, GitHubError> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        // alloc: error path
        let errors: Vec<String> = found.errors.iter().map(ToString::to_string).collect();
        return Err(GitHubError::new(format!( // alloc: error path
            "no root certificate to verify GitHub with was found on this machine ({})",
            // alloc: error path
            if errors.is_empty() { "no errors loading them".to_owned() } else { errors.join("; ") },
        )));
    }
    // alloc: once, at start, the TLS config
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| GitHubError::new(format!("TLS versions: {error}")))? // alloc: error path
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config)) // alloc: once, at start, the TLS config
}

fn now() -> Result<u64, GitHubError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|it| it.as_secs())
        .map_err(|_| GitHubError::new("the clock is before 1970"))
}

/// An RFC 3339 time in UTC (`2016-07-11T22:14:10Z`, fractions of a second allowed) as Unix
/// seconds. Another offset is refused.
fn unix_seconds(time: &str) -> Result<u64, GitHubError> {
    // alloc: error path
    let refuse = |why: &str| GitHubError::new(format!("expires_at {time:?} {why}"));
    let bytes = time.as_bytes();
    if bytes.len() < 19 {
        return Err(refuse("is not an RFC 3339 time"));
    }
    let digits = |at: usize, len: usize| -> Result<u64, GitHubError> {
        let part = &bytes[at..at + len];
        if !part.iter().all(u8::is_ascii_digit) {
            return Err(refuse("is not an RFC 3339 time"));
        }
        Ok(part.iter().fold(0, |sum, digit| sum * 10 + u64::from(digit - b'0')))
    };
    for (at, separator) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if bytes[at] != separator && !(at == 10 && bytes[at] == b't') {
            return Err(refuse("is not an RFC 3339 time"));
        }
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    let mut rest = &bytes[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let len = fraction.iter().take_while(|it| it.is_ascii_digit()).count();
        if len == 0 {
            return Err(refuse("has a fraction of a second with no digits"));
        }
        rest = &fraction[len..];
    }
    match rest {
        b"Z" | b"z" => {}
        b"" => return Err(refuse("has no offset; GitHub's are UTC (Z)")),
        _ => return Err(refuse("is not in UTC (Z); no other offset is taken")),
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(refuse("has no such month")),
    };
    if year < 1970 || day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return Err(refuse("is no such time"));
    }
    Ok(days_since_epoch(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar, from 1970 on.
fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Deserialize)]
struct MintedToken<'a> {
    token: String,
    #[serde(borrow)]
    expires_at: Cow<'a, str>,
}

#[derive(Deserialize)]
struct Created {
    id: u64,
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

#[cfg(test)]
mod tests;
