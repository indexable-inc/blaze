//! Fastest GitHub & Git wire transports for `bgit`:
//!
//! 1. **HTTPS / TLS 1.3 + HTTP/2 (`blaze-h2`, ALPN `h2`)** over a single persistent TCP+TLS
//!    connection (`rustls` + native OS trust store, 16 MiB stream / 32 MiB conn flow-control
//!    windows, HPACK-sensitive `Authorization`, `Accept-Encoding: gzip`, redirect following).
//!    Reuses the exact same TLS 1.3 + HTTP/2 session across `info/refs` → `ls-refs` → `fetch`
//!    (saving 2 full TCP+TLS handshakes vs naive per-request clients!).
//! 2. **HTTP/1.1 Keep-Alive** (over TLS 1.3 or plain `http://`) when the server negotiates
//!    `http/1.1` (e.g. local `bgit serve --http`).
//! 3. **Native SSH Protocol v2** (`git@github.com:org/repo.git` & `ssh://...`) over a single
//!    persistent `ssh -o SendEnv=GIT_PROTOCOL` stdio session (`GIT_PROTOCOL=version=2`).

use std::{
    fs,
    io::{self, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant as StdInstant},
};

use blaze_base64::STANDARD as BASE64_STD;
use blaze_core::trust::{Places as TrustPlaces, system as load_system_trust_roots};
use blaze_deflate::{Limits as DeflateLimits, gunzip};
use blaze_h2::{
    Connection as H2Conn, Event as H2Event, Field as H2Field, Instant as H2Instant,
    Limits as H2Limits, RequestHead as H2RequestHead,
};
use blaze_own::{Alloc, Vec as OwnVec};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, StreamOwned,
    pki_types::{CertificateDer, ServerName},
};

/// Parsed remote repository URL (`https://`, `http://`, `git@host:path`, `ssh://`, `github:o/r`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteUrl {
    Http(HttpTarget),
    Ssh(SshTarget),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpTarget {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub base_path: String,
    pub creds: Option<(String, String)>,
}

impl HttpTarget {
    pub(crate) fn authority(&self) -> String {
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub(crate) fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshTarget {
    /// `user@host` or `host` passed to `ssh`.
    pub destination: String,
    pub port: Option<u16>,
    /// Repository path passed to `git-upload-pack` / `git-receive-pack` (e.g. `owner/repo.git`).
    pub repo_path: String,
}

/// Parses `https://`, `http://`, `ssh://`, SCP-style `git@host:path`, or `github:owner/repo`.
pub(crate) fn parse_remote_url(raw: &str) -> io::Result<RemoteUrl> {
    let trimmed = raw.trim();
    if let Some(slug) = trimmed.strip_prefix("github:") {
        let clean = slug.trim_start_matches('/').trim_end_matches('/');
        let base_path = if clean.ends_with(".git") {
            format!("/{clean}")
        } else {
            format!("/{clean}.git")
        };
        return Ok(RemoteUrl::Http(HttpTarget {
            tls: true,
            host: "github.com".to_owned(),
            port: 443,
            base_path,
            creds: None,
        }));
    }

    if let Some(rest) = trimmed.strip_prefix("https://") {
        return Ok(RemoteUrl::Http(parse_http_target(rest, true)?));
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return Ok(RemoteUrl::Http(parse_http_target(rest, false)?));
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://") {
        let (auth_host, path_part) = rest.split_once('/').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid ssh:// URL: {raw}"))
        })?;
        let (destination, port) = if let Some((h, p_str)) = auth_host.rsplit_once(':')
            && let Ok(p) = p_str.parse::<u16>()
        {
            (h.to_owned(), Some(p))
        } else {
            (auth_host.to_owned(), None)
        };
        let repo_path = path_part.trim_start_matches('/').to_owned();
        return Ok(RemoteUrl::Ssh(SshTarget { destination, port, repo_path }));
    }

    // SCP-style SSH URL: `git@github.com:owner/repo.git`
    if !trimmed.contains("://")
        && let Some((dest, path)) = trimmed.split_once(':')
        && dest.contains('@')
        && !dest.contains('/')
        && !path.is_empty()
    {
        return Ok(RemoteUrl::Ssh(SshTarget {
            destination: dest.to_owned(),
            port: None,
            repo_path: path.to_owned(),
        }));
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "unsupported remote URL `{raw}` (expected https://, http://, git@host:org/repo.git, ssh://, or github:org/repo)"
        ),
    ))
}

/// Returns `true` if `s` looks like a direct remote URL rather than a configured remote name.
pub(crate) fn looks_like_remote_url(s: &str) -> bool {
    s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("ssh://")
        || s.starts_with("github:")
        || (!s.contains("://") && s.contains('@') && s.contains(':'))
}

/// Derives a default destination folder name from a clone URL (e.g. `https://github.com/org/foo.git` -> `foo`).
pub(crate) fn default_clone_dir_name(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("repo")
        .strip_suffix(".git")
        .unwrap_or_else(|| trimmed.rsplit(['/', ':']).next().unwrap_or("repo"));
    if tail.is_empty() { "repo".to_owned() } else { tail.to_owned() }
}

fn parse_http_target(rest: &str, tls: bool) -> io::Result<HttpTarget> {
    let (authority_raw, path_raw) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let (creds, host_port) = match authority_raw.rsplit_once('@') {
        Some((user_info, hp)) => {
            let pair = match user_info.split_once(':') {
                Some((u, p)) => (u.to_owned(), p.to_owned()),
                None => (user_info.to_owned(), String::new()),
            };
            (Some(pair), hp)
        }
        None => (None, authority_raw),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some((h, p_str)) = host_port.rsplit_once(':')
        && !h.contains(']')
        && let Ok(p) = p_str.parse::<u16>()
    {
        (h.to_owned(), p)
    } else {
        (host_port.to_owned(), default_port)
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty HTTP host in URL"));
    }
    let base_path = path_raw.trim_end_matches('/').to_owned();
    Ok(HttpTarget { tls, host, port, base_path, creds })
}

// ---------------------------------------------------------------------------
// Cached TLS 1.3 ClientConfig (ALPN h2 + http/1.1) & GitHub credential lookup
// ---------------------------------------------------------------------------

fn tls_client_config() -> io::Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let res = CONFIG.get_or_init(|| {
        // Dogfood `blaze_core::trust::system`: zero link-time Security.framework dependency
        // (`dlopen` only on first HTTPS connect), honors macOS admin/user trust settings and
        // Linux `SSL_CERT_FILE`/`SSL_CERT_DIR`, never bakes in a static CA list.
        let cert_file = std::env::var_os("SSL_CERT_FILE");
        let cert_dir = std::env::var_os("SSL_CERT_DIR");
        let places = TrustPlaces {
            cert_file: cert_file.as_deref(),
            cert_dir: cert_dir.as_deref(),
        };
        let trust_roots = load_system_trust_roots(places, Alloc::global())
            .map_err(|e| format!("blaze_core::trust::system: {e}"))?;
        let mut roots = RootCertStore::empty();
        for der in trust_roots.each() {
            let _ = roots.add(CertificateDer::from_slice(der).into_owned());
        }
        if roots.is_empty() {
            return Err("no OS root CA certificates found for TLS".to_owned());
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("rustls protocol versions: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Negotiate HTTP/2 first (GitHub's fastest Smart HTTP transport), falling back to HTTP/1.1.
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    });
    match res {
        Ok(arc) => Ok(Arc::clone(arc)),
        Err(msg) => Err(io::Error::other(msg.clone())),
    }
}

fn encode_basic_auth(user: &str, pass: &str) -> String {
    let raw = format!("{user}:{pass}");
    let encoded = BASE64_STD.encode_in(raw.as_bytes(), Alloc::global());
    format!("Basic {}", encoded.as_str())
}

/// Resolves credentials for `target` without touching external `git`:
/// 1. Embedded URL `user:token@`
/// 2. `GITHUB_TOKEN` / `GH_TOKEN` env var (for `github.com`)
/// 3. `~/.git-credentials`
/// 4. `~/.config/gh/hosts.yml` (`oauth_token:` for `github.com`)
fn resolve_initial_auth(target: &HttpTarget) -> Option<String> {
    if let Some((u, p)) = &target.creds {
        return Some(encode_basic_auth(u, p));
    }
    let is_github = target.host.eq_ignore_ascii_case("github.com");
    if is_github {
        if let Ok(tok) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN"))
            && !tok.trim().is_empty()
        {
            return Some(encode_basic_auth("x-access-token", tok.trim()));
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(raw) = fs::read_to_string(home.join(".git-credentials")) {
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Ok(RemoteUrl::Http(cred_t)) = parse_remote_url(line)
                    && cred_t.host.eq_ignore_ascii_case(&target.host)
                    && let Some((u, p)) = cred_t.creds
                {
                    return Some(encode_basic_auth(&u, &p));
                }
            }
        }
        if is_github {
            let gh_hosts = home.join(".config/gh/hosts.yml");
            if let Ok(raw) = fs::read_to_string(gh_hosts) {
                let mut in_github = false;
                for line in raw.lines() {
                    let trimmed = line.trim();
                    if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.ends_with(':') {
                        in_github = trimmed == "github.com:";
                        continue;
                    }
                    if in_github
                        && let Some(rest) = trimmed.strip_prefix("oauth_token:")
                    {
                        let tok = rest.trim().trim_matches('"').trim_matches('\'');
                        if !tok.is_empty() {
                            return Some(encode_basic_auth("x-access-token", tok));
                        }
                    }
                }
            }
        }
    }
    None
}

/// On HTTP 401 from `github.com`, queries `gh auth token` (reads macOS Keychain token if user is signed into `gh`).
fn resolve_gh_cli_auth(host: &str) -> Option<String> {
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let out = Command::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if tok.is_empty() {
        None
    } else {
        Some(encode_basic_auth("x-access-token", &tok))
    }
}

// ---------------------------------------------------------------------------
// Persistent Smart HTTP/2 (blaze-h2) + TLS 1.3 + HTTP/1.1 Keep-Alive Session
// ---------------------------------------------------------------------------

struct HttpReply {
    status: u16,
    location: Option<String>,
    content_encoding: Option<String>,
    body: Vec<u8>,
}

enum LiveConn {
    H2 {
        tls: StreamOwned<ClientConnection, TcpStream>,
        h2: H2Conn<'static>,
        epoch: StdInstant,
    },
    H1Tls(StreamOwned<ClientConnection, TcpStream>),
    H1Plain(TcpStream),
}

/// Persistent Smart HTTP client session reusing ONE TCP + TLS 1.3 + HTTP/2 connection across
/// the entire Git Protocol v2 handshake (`info/refs` → `ls-refs` → `fetch` / `receive-pack`).
pub(crate) struct SmartHttpSession {
    pub target: HttpTarget,
    auth: Option<String>,
    tried_gh_cli_auth: bool,
    conn: Option<LiveConn>,
    negotiated_proto: &'static str,
}

impl SmartHttpSession {
    pub(crate) fn new(target: HttpTarget) -> Self {
        let auth = resolve_initial_auth(&target);
        let negotiated_proto = if target.tls { "https" } else { "http/1.1" };
        SmartHttpSession {
            target,
            auth,
            tried_gh_cli_auth: false,
            conn: None,
            negotiated_proto,
        }
    }

    pub(crate) fn proto_label(&self) -> &'static str {
        self.negotiated_proto
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let tcp = TcpStream::connect(self.target.socket_addr())?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(60)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(60)));

        if !self.target.tls {
            self.negotiated_proto = "http/1.1";
            self.conn = Some(LiveConn::H1Plain(tcp));
            return Ok(());
        }

        let cfg = tls_client_config()?;
        let server_name = ServerName::try_from(self.target.host.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid TLS host `{}`", self.target.host)))?;
        let client = ClientConnection::new(cfg, server_name)
            .map_err(|e| io::Error::other(format!("rustls client: {e}")))?;
        let mut tls = StreamOwned::new(client, tcp);
        // Complete TLS 1.3 handshake upfront so ALPN protocol (`h2` vs `http/1.1`) is known immediately!
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock)?;
        }
        let alpn_h2 = tls.conn.alpn_protocol() == Some(b"h2".as_slice());
        if alpn_h2 {
            let limits = H2Limits {
                stream_window: 16 << 20,
                conn_window: 32 << 20,
                frame_size: 64 << 10,
                out_buffer: 4 << 20,
                ..H2Limits::DEFAULT
            };
            let h2 = H2Conn::client(limits, H2Instant::ZERO, Alloc::global())
                .map_err(|e| io::Error::other(format!("blaze-h2 init: {e}")))?;
            self.negotiated_proto = "h2+tls1.3";
            self.conn = Some(LiveConn::H2 { tls, h2, epoch: StdInstant::now() });
        } else {
            self.negotiated_proto = "http/1.1+tls";
            self.conn = Some(LiveConn::H1Tls(tls));
        }
        Ok(())
    }

    /// Executes one HTTP request (following redirects and retrying once on 401 with `gh auth token`).
    pub(crate) fn exchange(
        &mut self,
        method: &str,
        suffix_path_and_query: &str,
        content_type: Option<&str>,
        git_v2: bool,
        body: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut redirects = 0u8;
        loop {
            let full_path = format!("{}{}", self.target.base_path, suffix_path_and_query);
            let reply = self.exchange_once(method, &full_path, content_type, git_v2, body)?;
            match reply.status {
                200 => {
                    if reply
                        .content_encoding
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case("gzip"))
                    {
                        let mut inflated = OwnVec::new_in(Alloc::global());
                        let limits = DeflateLimits {
                            most_out: 512 << 20,
                            most_ratio: 1024,
                            most_header: 64 << 10,
                        };
                        gunzip(&reply.body, limits, Alloc::global(), &mut inflated)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("gzip decode: {e}")))?;
                        return Ok(inflated.as_slice().to_vec());
                    }
                    return Ok(reply.body);
                }
                301 | 302 | 303 | 307 | 308 if redirects < 5 => {
                    redirects += 1;
                    let loc = reply.location.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "HTTP redirect missing Location header")
                    })?;
                    if let Ok(RemoteUrl::Http(mut next_target)) = parse_remote_url(&loc) {
                        // Strip the suffix from redirected full URL to get the new `base_path`.
                        if let Some(stripped) = next_target.base_path.strip_suffix(suffix_path_and_query) {
                            next_target.base_path = stripped.to_owned();
                        } else if let Some(q_idx) = suffix_path_and_query.find('?')
                            && let Some(stripped) =
                                next_target.base_path.strip_suffix(&suffix_path_and_query[..q_idx])
                        {
                            next_target.base_path = stripped.to_owned();
                        }
                        if next_target.host != self.target.host
                            || next_target.port != self.target.port
                            || next_target.tls != self.target.tls
                        {
                            self.conn = None;
                            self.auth = resolve_initial_auth(&next_target);
                        }
                        self.target = next_target;
                    } else {
                        return Err(io::Error::other(format!("unsupported redirect target: {loc}")));
                    }
                }
                401 if !self.tried_gh_cli_auth => {
                    self.tried_gh_cli_auth = true;
                    if let Some(gh_auth) = resolve_gh_cli_auth(&self.target.host) {
                        self.auth = Some(gh_auth);
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "HTTP 401 Unauthorized from {} (set GITHUB_TOKEN / GH_TOKEN or sign in with `gh auth login`)",
                            self.target.host
                        ),
                    ));
                }
                status => {
                    let snippet = String::from_utf8_lossy(&reply.body[..reply.body.len().min(200)]);
                    return Err(io::Error::other(format!(
                        "HTTP {status} from {}{full_path}: {}",
                        self.target.authority(),
                        snippet.trim()
                    )));
                }
            }
        }
    }

    fn exchange_once(
        &mut self,
        method: &str,
        full_path: &str,
        content_type: Option<&str>,
        git_v2: bool,
        body: &[u8],
    ) -> io::Result<HttpReply> {
        // Retry once transparently if a reused keep-alive connection was closed by peer.
        for attempt in 0..2 {
            let was_reused = self.conn.is_some();
            self.ensure_connected()?;
            let authority = self.target.authority();
            let auth_hdr = self.auth.as_deref();
            let res = match self.conn.as_mut().expect("connected") {
                LiveConn::H2 { tls, h2, epoch } => h2_exchange(
                    tls,
                    h2,
                    *epoch,
                    method,
                    &authority,
                    full_path,
                    content_type,
                    git_v2,
                    auth_hdr,
                    body,
                ),
                LiveConn::H1Tls(tls) => h1_exchange(
                    tls,
                    method,
                    &authority,
                    full_path,
                    content_type,
                    git_v2,
                    auth_hdr,
                    body,
                ),
                LiveConn::H1Plain(tcp) => h1_exchange(
                    tcp,
                    method,
                    &authority,
                    full_path,
                    content_type,
                    git_v2,
                    auth_hdr,
                    body,
                ),
            };
            match res {
                Ok((reply, keep_alive)) => {
                    if !keep_alive {
                        self.conn = None;
                    }
                    return Ok(reply);
                }
                Err(e) if was_reused && attempt == 0 => {
                    self.conn = None;
                    let _ = e;
                    continue;
                }
                Err(e) => {
                    self.conn = None;
                    return Err(e);
                }
            }
        }
        unreachable!()
    }
}

#[allow(clippy::too_many_arguments)]
fn h2_exchange(
    tls: &mut StreamOwned<ClientConnection, TcpStream>,
    h2: &mut H2Conn<'static>,
    epoch: StdInstant,
    method: &str,
    authority: &str,
    full_path: &str,
    content_type: Option<&str>,
    git_v2: bool,
    auth: Option<&str>,
    body: &[u8],
) -> io::Result<(HttpReply, bool)> {
    let mut fields: Vec<H2Field<'_>> = Vec::with_capacity(6);
    fields.push(H2Field::new(b"user-agent", b"git/2.47.0 blaze-git/1"));
    fields.push(H2Field::new(b"accept", b"*/*"));
    fields.push(H2Field::new(b"accept-encoding", b"gzip"));
    if git_v2 {
        fields.push(H2Field::new(b"git-protocol", b"version=2"));
    }
    if let Some(ct) = content_type {
        fields.push(H2Field::new(b"content-type", ct.as_bytes()));
    }
    if let Some(a) = auth {
        fields.push(H2Field::sensitive(b"authorization", a.as_bytes()));
    }

    let head = H2RequestHead {
        method: method.as_bytes(),
        scheme: Some(b"https"),
        authority: Some(authority.as_bytes()),
        path: Some(full_path.as_bytes()),
        fields: &fields,
    };
    let end_on_head = body.is_empty();
    let stream_id = h2
        .request(&head, end_on_head)
        .map_err(|e| io::Error::other(format!("h2 request: {e}")))?;

    let mut sent = 0usize;
    let mut status = 0u16;
    let mut location: Option<String> = None;
    let mut content_encoding: Option<String> = None;
    let mut resp_body: Vec<u8> = Vec::new();
    let mut stream_done = false;
    let mut keep_alive = true;
    let mut tx_buf = [0u8; 32_768];
    let mut rx_buf = [0u8; 65_536];
    let mut data_buf = [0u8; 65_536];

    while !stream_done {
        // 1. Feed remaining request body chunks within flow-control window.
        while sent < body.len() {
            let remaining = &body[sent..];
            let chunk = &remaining[..remaining.len().min(65_536)];
            let is_last = sent + chunk.len() == body.len();
            let n = h2
                .send_body(stream_id, chunk, is_last)
                .map_err(|e| io::Error::other(format!("h2 send_body: {e}")))?;
            if n == 0 {
                break;
            }
            sent += n;
        }

        // 2. Flush all pending outbound HTTP/2 frames over TLS 1.3.
        loop {
            let n = h2.transmit(&mut tx_buf);
            if n == 0 {
                break;
            }
            tls.write_all(&tx_buf[..n])?;
        }
        tls.flush()?;

        // 3. Read incoming TLS bytes and feed into `blaze-h2`.
        let n_read = tls.read(&mut rx_buf)?;
        if n_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TLS peer closed HTTP/2 stream before response completed",
            ));
        }
        let now = H2Instant::from_nanos(epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        h2.received(&rx_buf[..n_read], now)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("h2 received: {e}")))?;

        // 4. Drain all HTTP/2 events and stream body bytes (emitting WINDOW_UPDATEs!).
        while let Some(ev) = h2.poll_event() {
            match ev {
                H2Event::Headers { stream, fields: hdrs, end } if stream == stream_id => {
                    for i in 0..hdrs.len() {
                        if let Some(f) = hdrs.get(i) {
                            if f.name == b":status" {
                                if let Ok(s) = core::str::from_utf8(f.value)
                                    && let Ok(code) = s.parse::<u16>()
                                {
                                    status = code;
                                }
                            } else if f.name == b"location" {
                                location = Some(String::from_utf8_lossy(f.value).into_owned());
                            } else if f.name == b"content-encoding" {
                                content_encoding = Some(String::from_utf8_lossy(f.value).into_owned());
                            }
                        }
                    }
                    if end {
                        stream_done = true;
                    }
                }
                H2Event::Data { stream } if stream == stream_id => {
                    while h2.stream_state(stream_id).is_some() {
                        let r = h2
                            .read(stream_id, &mut data_buf)
                            .map_err(|e| io::Error::other(format!("h2 read: {e}")))?;
                        if r == 0 {
                            break;
                        }
                        resp_body.extend_from_slice(&data_buf[..r]);
                    }
                }
                H2Event::End { stream } if stream == stream_id => {
                    while h2.stream_state(stream_id).is_some() {
                        let r = h2
                            .read(stream_id, &mut data_buf)
                            .map_err(|e| io::Error::other(format!("h2 read end: {e}")))?;
                        if r == 0 {
                            break;
                        }
                        resp_body.extend_from_slice(&data_buf[..r]);
                    }
                    stream_done = true;
                }
                H2Event::Reset { stream, code, .. } if stream == stream_id => {
                    return Err(io::Error::other(format!("HTTP/2 stream reset: {code:?}")));
                }
                H2Event::GoAway { .. } => {
                    keep_alive = false;
                }
                _ => {}
            }
        }
    }

    // Flush any final SETTINGS ACK / WINDOW_UPDATE frames.
    loop {
        let n = h2.transmit(&mut tx_buf);
        if n == 0 {
            break;
        }
        let _ = tls.write_all(&tx_buf[..n]);
    }
    let _ = tls.flush();

    Ok((
        HttpReply { status, location, content_encoding, body: resp_body },
        keep_alive && !h2.is_done(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn h1_exchange<S: Read + Write>(
    stream: &mut S,
    method: &str,
    authority: &str,
    full_path: &str,
    content_type: Option<&str>,
    git_v2: bool,
    auth: Option<&str>,
    body: &[u8],
) -> io::Result<(HttpReply, bool)> {
    let mut req = Vec::with_capacity(320 + body.len());
    write!(
        req,
        "{method} {full_path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: git/2.47.0 blaze-git/1\r\nAccept: */*\r\nAccept-Encoding: gzip\r\nConnection: keep-alive\r\n"
    )?;
    if git_v2 {
        req.extend_from_slice(b"Git-Protocol: version=2\r\n");
    }
    if let Some(ct) = content_type {
        write!(req, "Content-Type: {ct}\r\n")?;
    }
    if let Some(a) = auth {
        write!(req, "Authorization: {a}\r\n")?;
    }
    if !body.is_empty() || method == "POST" {
        write!(req, "Content-Length: {}\r\n", body.len())?;
    }
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(body);
    stream.write_all(&req)?;
    stream.flush()?;

    // Read response header byte-by-byte until `\r\n\r\n` so we never over-read a keep-alive next response.
    let mut hdr_buf = Vec::with_capacity(512);
    let mut b = [0u8; 1];
    loop {
        let n = stream.read(&mut b)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF reading HTTP/1.1 headers"));
        }
        hdr_buf.push(b[0]);
        if hdr_buf.len() >= 4 && &hdr_buf[hdr_buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if hdr_buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP/1.1 headers exceed 64 KiB"));
        }
    }

    let head = core::str::from_utf8(&hdr_buf[..hdr_buf.len() - 4])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 HTTP/1.1 headers"))?;
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line: {status_line}")))?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut close_conn = false;
    let mut location: Option<String> = None;
    let mut content_encoding: Option<String> = None;

    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let vt = v.trim();
            match kl.as_str() {
                "content-length" => content_length = vt.parse::<usize>().ok(),
                "transfer-encoding" => {
                    if vt.to_ascii_lowercase().contains("chunked") {
                        is_chunked = true;
                    }
                }
                "connection" => {
                    if vt.eq_ignore_ascii_case("close") {
                        close_conn = true;
                    }
                }
                "location" => location = Some(vt.to_owned()),
                "content-encoding" => content_encoding = Some(vt.to_owned()),
                _ => {}
            }
        }
    }

    let resp_body = if is_chunked {
        read_chunked_body(stream)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)?;
        buf
    } else {
        close_conn = true;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf)?;
        buf
    };

    Ok((
        HttpReply { status, location, content_encoding, body: resp_body },
        !close_conn,
    ))
}

fn read_chunked_body<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut line = Vec::with_capacity(32);
    let mut b = [0u8; 1];
    loop {
        line.clear();
        loop {
            r.read_exact(&mut b)?;
            line.push(b[0]);
            if line.len() >= 2 && &line[line.len() - 2..] == b"\r\n" {
                break;
            }
        }
        let len_s = core::str::from_utf8(&line[..line.len() - 2])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk len utf8"))?;
        let n = usize::from_str_radix(len_s.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk hex"))?;
        if n == 0 {
            // Drain trailing CRLF (and optional empty trailers).
            loop {
                line.clear();
                loop {
                    r.read_exact(&mut b)?;
                    line.push(b[0]);
                    if line.len() >= 2 && &line[line.len() - 2..] == b"\r\n" {
                        break;
                    }
                }
                if line == b"\r\n" {
                    break;
                }
            }
            break;
        }
        let prev = out.len();
        out.resize(prev + n, 0);
        r.read_exact(&mut out[prev..])?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Native SSH Protocol v2 Session (`git@github.com:org/repo.git` & `ssh://...`)
// ---------------------------------------------------------------------------

/// Single persistent SSH stdio session speaking Git Protocol v2 (`GIT_PROTOCOL=version=2`).
pub(crate) struct SshSession {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl SshSession {
    pub(crate) fn connect(target: &SshTarget, service: &str, git_v2: bool) -> io::Result<Self> {
        let mut cmd = Command::new("ssh");
        if git_v2 {
            cmd.env("GIT_PROTOCOL", "version=2");
            cmd.args(["-o", "SendEnv=GIT_PROTOCOL"]);
        }
        if let Some(port) = target.port {
            cmd.args(["-p", &port.to_string()]);
        }
        let escaped_path = target.repo_path.replace('\'', "'\\''");
        let remote_cmd = format!("{service} '{escaped_path}'");
        cmd.arg(&target.destination)
            .arg(remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| {
            io::Error::other(format!("failed to spawn ssh to {}: {e}", target.destination))
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Ok(SshSession { child, stdin, stdout })
    }

    /// Reads raw Git pkt-line stream until `0000` flush (preserving `0001` delim & `0002` end).
    pub(crate) fn read_pkt_until_flush(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut hdr = [0u8; 4];
        loop {
            self.stdout.read_exact(&mut hdr)?;
            out.extend_from_slice(&hdr);
            let hex = core::str::from_utf8(&hdr)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad ssh pkt header"))?;
            let len = usize::from_str_radix(hex, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad ssh pkt len `{hex}`")))?;
            if len == 0 {
                return Ok(out);
            }
            if len == 1 || len == 2 {
                continue;
            }
            if len < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid ssh pkt len"));
            }
            let prev = out.len();
            out.resize(prev + (len - 4), 0);
            self.stdout.read_exact(&mut out[prev..])?;
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes)?;
        self.stdin.flush()
    }

    pub(crate) fn read_to_end(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.stdout.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Gracefully terminates the Protocol v2 SSH session with a final flush packet.
    pub(crate) fn finish(mut self, send_flush: bool) -> io::Result<()> {
        if send_flush {
            let _ = self.stdin.write_all(b"0000");
            let _ = self.stdin.flush();
        }
        drop(self.stdin);
        let status = self.child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!("ssh exited with {status}")));
        }
        Ok(())
    }
}
