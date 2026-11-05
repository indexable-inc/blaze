//! Proxies, sans-IO: which proxy a connection to an origin goes through ([`Proxies`], read from
//! the variables curl and cargo read by [`Proxies::from_vars`]), and the handshake that opens a
//! tunnel through one to the origin ([`Handshake`]): HTTP `CONNECT` (RFC 9110 section 9.3.6),
//! SOCKS5 (RFC 1928, with RFC 1929's user and password) and SOCKS4 and 4a. A plain `http`
//! request through an HTTP proxy needs no tunnel: it is sent to the proxy in absolute form (RFC
//! 9112 section 3.2.2), as curl sends it ([`crate::Client`] does that).
//!
//! # What is read
//!
//! As curl reads it, with cargo's `http.proxy` over it:
//!
//! - `CARGO_HTTP_PROXY`, set, is every request's proxy; set and empty, none is (cargo's way to
//!   turn the inherited ones off). `no_proxy` still holds.
//! - Otherwise `https_proxy`, then `HTTPS_PROXY`, for an `https` origin; `http_proxy`, then
//!   `HTTP_PROXY`, for an `http` one; `all_proxy`, then `ALL_PROXY`, for either when its own is
//!   not set. The lower-case name wins; an empty value is not set.
//! - Under CGI (`REQUEST_METHOD` set) `HTTP_PROXY` is the request's `Proxy` header, not
//!   configuration (httpoxy), so only `http_proxy` is read; on Windows, where names are
//!   case-insensitive and `http_proxy` is that same variable, neither is. curl reads no
//!   `HTTP_PROXY` at all; it is read here outside CGI, as cargo's and most clients read it.
//! - `no_proxy`, then `NO_PROXY`: names, addresses and CIDR ranges, split by commas or spaces,
//!   asked directly. A name is itself and its subdomains (a leading `.` or `*.` says the same);
//!   `*` is every origin.
//!
//! A proxy is `[scheme://][user[:password]@]host[:port]`: `http` when no scheme is said, the
//! port curl's 1080 when none is, the user and password percent-decoded.
//!
//! # What is refused
//!
//! Each naming the variable and never its value, which may hold a password ([`ProxyError`]): a
//! variable that is not Unicode, a value that is not an address, a scheme curl does not know,
//! an `https://` proxy (TLS to the proxy itself, which this client does not speak), a password
//! for SOCKS4 (which has none), a SOCKS5 user or password past 255 bytes, and a `NO_PROXY`
//! range that does not parse. None is dropped for going direct: a proxy named is a proxy used.

#![blaze::sans_io]

use std::{ffi::OsString, fmt, io::Write as _, net::IpAddr};

use base64::Engine as _;

use crate::h1;

/// A proxy's port when its address names none: curl's.
pub const DEFAULT_PORT: u16 = 1080;

/// How a proxy is spoken to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    /// `http://`, or no scheme: `CONNECT` for an `https` origin, absolute form for an `http` one.
    Http,
    /// `socks5h://` (`remote_dns`: the proxy resolves the origin's name), or `socks5://` (the
    /// name is resolved here and the proxy given an address).
    Socks5 { remote_dns: bool },
    /// `socks4a://` (`remote_dns`), or `socks4://` (resolved here, to an IPv4 address).
    Socks4 { remote_dns: bool },
}

impl ProxyKind {
    /// Its scheme, as a message says it.
    pub fn scheme(self) -> &'static str {
        match self {
            ProxyKind::Http => "http",
            ProxyKind::Socks5 { remote_dns: true } => "socks5h",
            ProxyKind::Socks5 { remote_dns: false } => "socks5",
            ProxyKind::Socks4 { remote_dns: true } => "socks4a",
            ProxyKind::Socks4 { remote_dns: false } => "socks4",
        }
    }

    /// Whether the origin's name is resolved here, not by the proxy.
    fn local_dns(self) -> bool {
        matches!(
            self,
            ProxyKind::Socks5 { remote_dns: false } | ProxyKind::Socks4 { remote_dns: false }
        )
    }
}

/// A proxy's user and password, percent-decoded. Never in a message or a `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    /// `None` when the address said none (`user@host`).
    pub password: Option<String>,
}

impl Credentials {
    /// `Proxy-Authorization`'s value: `Basic` with `user:password` (RFC 7617).
    pub fn basic(&self) -> String {
        let password = self.password.as_deref().unwrap_or_default();
        // alloc: one per connection through a proxy with a user, its header's value.
        let pair = format!("{}:{password}", self.user);
        // alloc: one per connection through a proxy with a user, its header's value.
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair))
    }
}

/// A proxy: how it is spoken to, where it is, and who asks.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    pub kind: ProxyKind,
    /// As the socket takes it: an IPv6 address without its brackets.
    pub host: String,
    pub port: u16,
    pub auth: Option<Credentials>,
}

impl Proxy {
    /// `host:port`, an IPv6 host in brackets: how a message names it.
    pub fn at(&self) -> String {
        authority(&self.host, self.port)
    }
}

/// The credentials left out: a `Config`'s `Debug` must not print a password.
impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The proxies a client's connections go through, by the origin's scheme, and the origins
/// that go direct.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proxies {
    /// For `http://` origins.
    pub http: Option<Proxy>,
    /// For `https://` origins.
    pub https: Option<Proxy>,
    pub no_proxy: NoProxy,
}

impl Proxies {
    /// No proxy: every connection direct.
    pub fn none() -> Proxies {
        Proxies::default()
    }

    /// The proxy a connection to `host` (an IPv6 address without its brackets), over `https`
    /// or plain `http`, goes through; `None` for one that goes direct.
    pub fn of(&self, https: bool, host: &str) -> Option<&Proxy> {
        let proxy = if https { self.https.as_ref() } else { self.http.as_ref() }?;
        (!self.no_proxy.matches(host)).then_some(proxy)
    }

    /// The proxies the variables `get` reads name, as curl and cargo read them (the module's
    /// documentation). `windows`: variable names are case-insensitive, so `http_proxy` is
    /// CGI's `HTTP_PROXY` too.
    pub fn from_vars(
        get: impl Fn(&str) -> Option<OsString>,
        windows: bool,
    ) -> Result<Proxies, ProxyError> {
        let read = |var: &'static str| -> Result<Option<String>, ProxyError> {
            let Some(raw) = get(var) else { return Ok(None) };
            let Ok(text) = raw.into_string() else {
                // alloc: an error's message, made only when it fails.
                return Err(ProxyError { var, why: "it is not Unicode".to_owned() });
            };
            // alloc: one per proxy variable set, read once as a client is made.
            Ok(Some(text.trim().to_owned()))
        };
        let first = |vars: &[&'static str]| -> Result<Option<(&'static str, String)>, ProxyError> {
            for &var in vars {
                if let Some(value) = read(var)?.filter(|it| !it.is_empty()) {
                    return Ok(Some((var, value)));
                }
            }
            Ok(None)
        };
        let no_proxy = match first(&["no_proxy", "NO_PROXY"])? {
            Some((var, value)) => NoProxy::parse(var, &value)?,
            None => NoProxy::default(),
        };
        // Its presence, empty or not, is what counts: empty turns the inherited ones off.
        if let Some(cargo) = read("CARGO_HTTP_PROXY")? {
            if cargo.is_empty() {
                return Ok(Proxies { http: None, https: None, no_proxy });
            }
            let proxy = parse_proxy("CARGO_HTTP_PROXY", &cargo)?;
            // alloc: the one proxy, for each scheme, once as a client is made.
            return Ok(Proxies { http: Some(proxy.clone()), https: Some(proxy), no_proxy });
        }
        let cgi = get("REQUEST_METHOD").is_some();
        let http_vars: &[&'static str] = match (cgi, windows) {
            (false, _) => &["http_proxy", "HTTP_PROXY"],
            (true, false) => &["http_proxy"],
            (true, true) => &[],
        };
        let all = first(&["all_proxy", "ALL_PROXY"])?;
        // alloc: `all_proxy`'s value for each scheme, once as a client is made.
        let http = first(http_vars)?.or_else(|| all.clone());
        let https = first(&["https_proxy", "HTTPS_PROXY"])?.or(all);
        let parse = |said: Option<(&'static str, String)>| {
            said.map(|(var, value)| parse_proxy(var, &value)).transpose()
        };
        Ok(Proxies { http: parse(http)?, https: parse(https)?, no_proxy })
    }
}

/// The origins asked directly (`no_proxy`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoProxy {
    /// `*`: every origin.
    all: bool,
    entries: Vec<Bypass>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Bypass {
    /// A name and its subdomains, in lower case, with no leading or trailing dot.
    Name(String),
    /// The addresses whose first `bits` are `net`'s: an address alone is all of its bits.
    Range { net: IpAddr, bits: u8 },
}

impl NoProxy {
    /// `value`, as `var` said it.
    fn parse(var: &'static str, value: &str) -> Result<NoProxy, ProxyError> {
        let mut no = NoProxy::default();
        for entry in value.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                no.all = true;
                continue;
            }
            let bare = entry.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(entry);
            let bypass = if let Some((net, bits)) = bare.split_once('/') {
                let net = net.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(net);
                let (Ok(net), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u8>()) else {
                    // alloc: an error's message, made only when it fails.
                    let why = format!("`{entry}` is no address range (address/bits)");
                    return Err(ProxyError { var, why });
                };
                let most = if net.is_ipv4() { 32 } else { 128 };
                if bits > most {
                    // alloc: an error's message, made only when it fails.
                    let why = format!("`{entry}` has more than the {most} bits of its address");
                    return Err(ProxyError { var, why });
                }
                Bypass::Range { net, bits }
            } else if let Ok(address) = bare.parse::<IpAddr>() {
                Bypass::Range { net: address, bits: if address.is_ipv4() { 32 } else { 128 } }
            } else {
                let name =
                    entry.trim_start_matches('*').trim_start_matches('.').trim_end_matches('.');
                if name.is_empty() {
                    continue;
                }
                // alloc: one per `no_proxy` name, read once as a client is made.
                Bypass::Name(name.to_ascii_lowercase())
            };
            no.entries.push(bypass);
        }
        Ok(no)
    }

    /// Whether `host` (a name, or an address without brackets) is asked directly.
    pub fn matches(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        let host = host.trim_end_matches('.');
        let address = host.parse::<IpAddr>().ok();
        self.entries.iter().any(|entry| match (entry, address) {
            (Bypass::Range { net, bits }, Some(address)) => within(address, *net, *bits),
            (Bypass::Range { .. }, None) => false,
            (Bypass::Name(name), _) => {
                host.eq_ignore_ascii_case(name)
                    || (host.len() > name.len()
                        && host.as_bytes()[host.len() - name.len() - 1] == b'.'
                        && host.as_bytes()[host.len() - name.len()..]
                            .eq_ignore_ascii_case(name.as_bytes()))
            }
        })
    }
}

/// Whether `address`'s first `bits` are `net`'s, both of one family.
fn within(address: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (address, net) {
        (IpAddr::V4(address), IpAddr::V4(net)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(address) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(address), IpAddr::V6(net)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(address) & mask == u128::from(net) & mask
        }
        (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

/// A proxy variable that can't be used: `var` names it, and `why` never holds its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyError {
    pub var: &'static str,
    pub why: String,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the proxy variable {}: {}", self.var, self.why)
    }
}

impl std::error::Error for ProxyError {}

/// `value`, the proxy `var` names (the module's documentation).
fn parse_proxy(var: &'static str, value: &str) -> Result<Proxy, ProxyError> {
    let refuse = |why: String| ProxyError { var, why };
    let (scheme, rest) = match value.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", value),
    };
    let kind = if scheme.eq_ignore_ascii_case("http") {
        ProxyKind::Http
    } else if scheme.eq_ignore_ascii_case("socks5h") {
        ProxyKind::Socks5 { remote_dns: true }
    } else if scheme.eq_ignore_ascii_case("socks5") {
        ProxyKind::Socks5 { remote_dns: false }
    } else if scheme.eq_ignore_ascii_case("socks4a") {
        ProxyKind::Socks4 { remote_dns: true }
    } else if scheme.eq_ignore_ascii_case("socks4") {
        ProxyKind::Socks4 { remote_dns: false }
    } else if scheme.eq_ignore_ascii_case("https") {
        return Err(refuse(
            // alloc: an error's message, made only when it fails.
            "an https:// proxy (TLS to the proxy itself) is not one this client speaks: name it http://"
                .to_owned(),
        ));
    } else {
        // alloc: an error's message, made only when it fails.
        return Err(refuse(format!(
            "`{scheme}://` is no proxy scheme (http, socks4, socks4a, socks5 or socks5h)"
        )));
    };
    // A scheme of no default port, so that one said (`:80`) is kept as said; url's messages
    // never quote what they parse.
    // alloc: one per proxy variable set, read once as a client is made.
    let url = url::Url::parse(&format!("proxy://{rest}"))
        // alloc: an error's message, made only when it fails.
        .map_err(|e| refuse(format!("it is not a proxy's address: {e}")))?;
    // alloc: one per proxy variable set, the host the driver resolves.
    let host = match url.host() {
        Some(url::Host::Domain(it)) if !it.is_empty() => it.to_owned(),
        Some(url::Host::Ipv4(it)) => it.to_string(),
        Some(url::Host::Ipv6(it)) => it.to_string(),
        // alloc: an error's message, made only when it fails.
        Some(url::Host::Domain(_)) | None => return Err(refuse("it names no host".to_owned())),
    };
    let port = url.port().unwrap_or(DEFAULT_PORT);
    let decode = |part: &str| {
        percent_encoding::percent_decode_str(part)
            .decode_utf8()
            // alloc: one per proxy credential, read once as a client is made.
            .map(|it| it.into_owned())
            // alloc: an error's message, made only when it fails.
            .map_err(|_| {
                refuse("its user or password is not UTF-8 once percent-decoded".to_owned())
            })
    };
    let user = decode(url.username())?;
    let password = url.password().map(decode).transpose()?;
    let auth = (!user.is_empty() || password.is_some()).then_some(Credentials { user, password });
    if let Some(auth) = &auth {
        match kind {
            ProxyKind::Socks4 { .. } if auth.password.is_some() => {
                return Err(refuse(
                    // alloc: an error's message, made only when it fails.
                    "SOCKS4 has no password, only a user: name it socks5:// or socks5h:// for a user and password"
                        .to_owned(),
                ));
            }
            ProxyKind::Socks4 { .. } if auth.user.as_bytes().contains(&0) => {
                // alloc: an error's message, made only when it fails.
                return Err(refuse("its user has a NUL in it, which SOCKS4 can't send".to_owned()));
            }
            ProxyKind::Socks5 { .. }
                if auth.user.len() > 255
                    || auth.password.as_ref().is_some_and(|it| it.len() > 255) =>
            {
                return Err(refuse(
                    // alloc: an error's message, made only when it fails.
                    "its user or password is past the 255 bytes SOCKS5 can send".to_owned(),
                ));
            }
            ProxyKind::Http | ProxyKind::Socks5 { .. } | ProxyKind::Socks4 { .. } => {}
        }
    }
    Ok(Proxy { kind, host, port, auth })
}

/// `host:port`, an IPv6 host in brackets.
pub(crate) fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        // alloc: one per message or request naming an address.
        format!("[{host}]:{port}")
    } else {
        // alloc: one per message or request naming an address.
        format!("{host}:{port}")
    }
}

/// Where a tunnel's handshake is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Waiting for the origin's addresses, to name it to the proxy by one (`socks4://`,
    /// `socks5://`).
    Resolving,
    /// `CONNECT` sent, its answer's head awaited.
    Connect,
    /// SOCKS5's greeting sent, the method the proxy picks awaited.
    Socks5Method,
    /// RFC 1929's user and password sent, their verdict awaited.
    Socks5Auth,
    /// SOCKS5's connect sent, its reply awaited.
    Socks5Reply,
    /// SOCKS4's connect sent, its 8-byte reply awaited.
    Socks4Reply,
    /// The tunnel is open: what follows is the origin's.
    Done,
}

/// A tunnel being opened through a proxy to an origin: bytes for the proxy come out
/// ([`Handshake::outgoing`]), its answers go in ([`Handshake::received`]), and once it is open
/// the connection is the origin's, its TLS (for `https`) spoken inside it.
pub struct Handshake {
    proxy: Proxy,
    /// The origin, as the socket takes it, and its port.
    host: String,
    port: u16,
    /// The origin's address, when it is one or was resolved: it is named by it, not its name.
    address: Option<IpAddr>,
    state: State,
    /// What came from the proxy and is not read yet.
    input: Vec<u8>,
    /// What is to go to the proxy.
    output: Vec<u8>,
}

impl Handshake {
    /// A tunnel through `proxy` to `host` (an IPv6 address without its brackets) at `port`.
    /// What to send first is ready at once, unless the origin's name must be resolved first
    /// ([`Handshake::resolving`], then [`Handshake::resolved`]).
    pub fn new(proxy: &Proxy, host: &str, port: u16) -> Result<Handshake, String> {
        let address = host.parse::<IpAddr>().ok();
        let mut it = Handshake {
            // alloc: one per connection through a proxy, the proxy it goes through.
            proxy: proxy.clone(),
            // alloc: one per connection through a proxy, the origin it leads to.
            host: host.to_owned(),
            port,
            address,
            state: State::Resolving,
            // alloc: empty until the proxy answers; a few bytes, once per connection.
            input: Vec::new(),
            // alloc: one per connection through a proxy, its handshake's bytes.
            output: Vec::new(),
        };
        if !(proxy.kind.local_dns() && address.is_none()) {
            it.open()?;
        }
        Ok(it)
    }

    /// The proxy it goes through.
    pub fn proxy(&self) -> &Proxy {
        &self.proxy
    }

    /// Whether it waits for the origin's addresses ([`Handshake::resolved`]) before anything
    /// is sent.
    pub fn resolving(&self) -> bool {
        self.state == State::Resolving
    }

    /// The origin's name resolved to `addresses`: it is named to the proxy by the first (the
    /// first IPv4 one, for SOCKS4).
    pub fn resolved(&mut self, addresses: &[IpAddr]) -> Result<(), String> {
        let socks4 = matches!(self.proxy.kind, ProxyKind::Socks4 { .. });
        let picked =
            if socks4 { addresses.iter().find(|it| it.is_ipv4()) } else { addresses.first() };
        let Some(address) = picked.copied() else {
            // alloc: an error's message, made only when it fails.
            return Err(format!(
                "{} resolves to no {}address, which a {}:// proxy is given in place of the name (socks{}:// gives it the name)",
                self.host,
                if socks4 { "IPv4 " } else { "" },
                self.proxy.kind.scheme(),
                if socks4 { "4a" } else { "5h" },
            ));
        };
        self.address = Some(address);
        self.open()
    }

    /// Adds what is to go to the proxy to `out`.
    pub fn outgoing(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.output);
    }

    /// Takes `bytes` from the proxy: `Some` with what came after the handshake (the origin's
    /// first bytes, usually none) once the tunnel is open, `None` while it waits for more.
    pub fn received(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.input.extend_from_slice(bytes);
        // Each turn reads one message whole and moves on, or returns for more: bounded by the
        // messages a handshake has.
        loop {
            match self.state {
                State::Resolving => {
                    // alloc: an error's message, made only when it fails.
                    return Err(format!(
                        "the proxy {} sent bytes before it was asked anything",
                        self.proxy.at()
                    ));
                }
                State::Done => return Ok(Some(std::mem::take(&mut self.input))),
                State::Connect => {
                    let head = match h1::parse_head(&self.input, false) {
                        Ok(Some(head)) => head,
                        Ok(None) => return Ok(None),
                        Err(why) => {
                            // alloc: an error's message, made only when it fails.
                            return Err(format!(
                                "the proxy {} answered CONNECT with {why}",
                                self.proxy.at()
                            ));
                        }
                    };
                    if !(200..300).contains(&head.status) {
                        let hint = match head.status {
                            407 => {
                                " (it wants credentials: name them in the proxy's address, http://user:password@host:port)"
                            }
                            _ => "",
                        };
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "the proxy {} refused to tunnel to {}: {}{hint}",
                            self.proxy.at(),
                            authority(&self.host, self.port),
                            head.status,
                        ));
                    }
                    self.input.drain(..head.len);
                    self.state = State::Done;
                }
                State::Socks5Method => {
                    let &[version, method, ..] = &self.input[..] else { return Ok(None) };
                    if version != 5 {
                        return Err(self.not_socks(5, version));
                    }
                    self.input.drain(..2);
                    match method {
                        0 => self.socks5_connect()?,
                        2 if self.proxy.auth.is_some() => self.socks5_auth()?,
                        0xff => {
                            // alloc: an error's message, made only when it fails.
                            return Err(format!(
                                "the SOCKS5 proxy {} takes none of the ways in asked ({})",
                                self.proxy.at(),
                                if self.proxy.auth.is_some() {
                                    "none, and a user and password"
                                } else {
                                    "none: it may want a user and password, socks5h://user:password@host:port"
                                },
                            ));
                        }
                        other => {
                            // alloc: an error's message, made only when it fails.
                            return Err(format!(
                                "the SOCKS5 proxy {} picked a way in that was not asked: {other:#04x}",
                                self.proxy.at()
                            ));
                        }
                    }
                }
                State::Socks5Auth => {
                    let &[_, status, ..] = &self.input[..] else { return Ok(None) };
                    self.input.drain(..2);
                    if status != 0 {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "the SOCKS5 proxy {} refused its user and password ({status:#04x})",
                            self.proxy.at()
                        ));
                    }
                    self.socks5_connect()?;
                }
                State::Socks5Reply => {
                    let &[version, reply, _, kind, first, ..] = &self.input[..] else {
                        return Ok(None);
                    };
                    if version != 5 {
                        return Err(self.not_socks(5, version));
                    }
                    if reply != 0 {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "the SOCKS5 proxy {} refused to connect to {}: {}",
                            self.proxy.at(),
                            authority(&self.host, self.port),
                            socks5_says(reply)
                        ));
                    }
                    // Its version, reply, a reserved byte and the address's kind, then the
                    // address it bound and that address's port.
                    let len = match kind {
                        1 => 4 + 4 + 2,
                        4 => 4 + 16 + 2,
                        3 => 4 + 1 + usize::from(first) + 2,
                        other => {
                            // alloc: an error's message, made only when it fails.
                            return Err(format!(
                                "the SOCKS5 proxy {} replied with an address of no kind there is: {other:#04x}",
                                self.proxy.at()
                            ));
                        }
                    };
                    if self.input.len() < len {
                        return Ok(None);
                    }
                    self.input.drain(..len);
                    self.state = State::Done;
                }
                State::Socks4Reply => {
                    if self.input.len() < 8 {
                        return Ok(None);
                    }
                    let (version, reply) = (self.input[0], self.input[1]);
                    if version != 0 {
                        return Err(self.not_socks(4, version));
                    }
                    if reply != 0x5a {
                        let says = match reply {
                            0x5b => "rejected or failed",
                            0x5c => "it can't reach the client's identd",
                            0x5d => "the client's identd does not confirm its user",
                            _ => "an answer SOCKS4 has no meaning for",
                        };
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "the SOCKS4 proxy {} refused to connect to {}: {says} ({reply:#04x})",
                            self.proxy.at(),
                            authority(&self.host, self.port),
                        ));
                    }
                    self.input.drain(..8);
                    self.state = State::Done;
                }
            }
        }
    }

    /// Why the proxy closing the connection now fails it.
    pub fn closed(&self) -> String {
        // alloc: an error's message, made only when it fails.
        format!(
            "the proxy {} closed the connection before the tunnel to {} was open",
            self.proxy.at(),
            authority(&self.host, self.port)
        )
    }

    /// The first message, now that the origin can be named.
    fn open(&mut self) -> Result<(), String> {
        match self.proxy.kind {
            ProxyKind::Http => {
                let to = authority(&self.host, self.port);
                // Writes into a vector, which cannot fail.
                _ = write!(self.output, "CONNECT {to} HTTP/1.1\r\nhost: {to}\r\n");
                if let Some(auth) = &self.proxy.auth {
                    _ = write!(self.output, "proxy-authorization: {}\r\n", auth.basic());
                }
                self.output.extend_from_slice(b"\r\n");
                self.state = State::Connect;
            }
            ProxyKind::Socks5 { .. } => {
                // Version 5, then the ways in offered: none, and a user and password when there
                // is one.
                if self.proxy.auth.is_some() {
                    self.output.extend_from_slice(&[5, 2, 0, 2]);
                } else {
                    self.output.extend_from_slice(&[5, 1, 0]);
                }
                self.state = State::Socks5Method;
            }
            ProxyKind::Socks4 { .. } => {
                // SOCKS4a names the origin by `0.0.0.x` (x not 0) and its name after the user.
                let (ip, name) = match self.address {
                    Some(IpAddr::V4(ip)) => (ip.octets(), None),
                    Some(IpAddr::V6(_)) => {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "the SOCKS4 proxy {} can't be given {}, an IPv6 address",
                            self.proxy.at(),
                            self.host
                        ));
                    }
                    None => ([0, 0, 0, 1], Some(self.host.as_bytes())),
                };
                self.output.extend_from_slice(&[4, 1]);
                self.output.extend_from_slice(&self.port.to_be_bytes());
                self.output.extend_from_slice(&ip);
                if let Some(auth) = &self.proxy.auth {
                    self.output.extend_from_slice(auth.user.as_bytes());
                }
                self.output.push(0);
                if let Some(name) = name {
                    self.output.extend_from_slice(name);
                    self.output.push(0);
                }
                self.state = State::Socks4Reply;
            }
        }
        Ok(())
    }

    /// RFC 1929's user and password, the way in the SOCKS5 proxy picked.
    fn socks5_auth(&mut self) -> Result<(), String> {
        let Some(auth) = &self.proxy.auth else {
            // alloc: an error's message, made only when it fails.
            return Err(format!(
                "the SOCKS5 proxy {} asks for a user and password",
                self.proxy.at()
            ));
        };
        let user = auth.user.as_bytes();
        let password = auth.password.as_deref().unwrap_or_default().as_bytes();
        let (Ok(user_len), Ok(password_len)) =
            (u8::try_from(user.len()), u8::try_from(password.len()))
        else {
            // alloc: an error's message, made only when it fails.
            return Err(format!(
                "the user or password for the proxy {} is past the 255 bytes SOCKS5 can send",
                self.proxy.at()
            ));
        };
        self.output.extend_from_slice(&[1, user_len]);
        self.output.extend_from_slice(user);
        self.output.push(password_len);
        self.output.extend_from_slice(password);
        self.state = State::Socks5Auth;
        Ok(())
    }

    /// SOCKS5's connect: the origin by its address, else by its name.
    fn socks5_connect(&mut self) -> Result<(), String> {
        self.output.extend_from_slice(&[5, 1, 0]);
        match self.address {
            Some(IpAddr::V4(ip)) => {
                self.output.push(1);
                self.output.extend_from_slice(&ip.octets());
            }
            Some(IpAddr::V6(ip)) => {
                self.output.push(4);
                self.output.extend_from_slice(&ip.octets());
            }
            None => {
                let name = self.host.as_bytes();
                let Ok(len) = u8::try_from(name.len()) else {
                    // alloc: an error's message, made only when it fails.
                    return Err(format!(
                        "{} is a name past the 255 bytes SOCKS5 can send",
                        self.host
                    ));
                };
                self.output.extend_from_slice(&[3, len]);
                self.output.extend_from_slice(name);
            }
        }
        self.output.extend_from_slice(&self.port.to_be_bytes());
        self.state = State::Socks5Reply;
        Ok(())
    }

    /// A reply whose version is not SOCKS `expected`'s.
    fn not_socks(&self, expected: u8, version: u8) -> String {
        // alloc: an error's message, made only when it fails.
        format!(
            "the proxy {} is not a SOCKS{expected} proxy: its reply's version is {version:#04x}",
            self.proxy.at()
        )
    }
}

/// A SOCKS5 reply's code, as RFC 1928 section 6 says it.
fn socks5_says(reply: u8) -> &'static str {
    match reply {
        1 => "general failure",
        2 => "its rules do not allow the connection",
        3 => "the network is unreachable",
        4 => "the host is unreachable",
        5 => "the connection was refused",
        6 => "the TTL expired",
        7 => "it does not take the command (CONNECT)",
        8 => "it does not take the address's kind",
        _ => "a reply RFC 1928 has no meaning for",
    }
}
