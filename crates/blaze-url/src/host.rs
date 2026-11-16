//! A host and a port, checked as it is made ([`HostPort`]); the check is a `const fn`
//! ([`refused`]) so bstd's `host!` runs it as the crate compiles ([`check_literal`]).

use alloc::{borrow::ToOwned, format, string::String, sync::Arc};
use core::{fmt, net::SocketAddr};

/// The most bytes in a host name (RFC 1035 section 2.3.4, without the root's dot).
pub const HOST_MOST: usize = 253;

/// A host and a port: a DNS name (labels of letters, digits and `-`, each 1 to 63 bytes and
/// neither starting nor ending in `-`), an IPv4 address, or an IPv6 address in brackets; then
/// a port from 1. Made by [`HostPort::new`], [`HostPort::parse`] or, for a literal, bstd's
/// `host!` as the crate compiles.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount and a copy
pub struct HostPort {
    host: Arc<str>,
    port: u16,
}

/// What [`HostPort::new`] or [`HostPort::parse`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct HostPortError {
    /// What was given: `host:port`, or the host and port joined so.
    pub given: String,
    pub why: HostPortWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostPortWhy {
    EmptyHost,
    /// More than [`HOST_MOST`] bytes.
    HostTooLong,
    /// A label is empty, longer than 63 bytes, or starts or ends with `-`.
    BadLabel,
    /// A byte that is in no host name or address.
    BadByte,
    /// A `[` with no `]`, or an IPv6 address with a byte other than hex digits, `:` and `.`.
    BadBrackets,
    /// No `:port` after the host.
    NoPort,
    /// The port is not a number from 1 to 65535.
    BadPort,
}

impl HostPortWhy {
    /// What is wrong, as a message says it after the name it is about; it never quotes the
    /// name (a proxy's address may hold a password).
    pub const fn says(self) -> &'static str {
        match self {
            HostPortWhy::EmptyHost => "has no host",
            HostPortWhy::HostTooLong => "has a host longer than 253 bytes",
            HostPortWhy::BadLabel => {
                "has a label that is empty, longer than 63 bytes, or starts or ends with `-`"
            }
            HostPortWhy::BadByte => {
                "has a byte that is no letter, digit, `-` or `.` in its host name"
            }
            HostPortWhy::BadBrackets => {
                "has an IPv6 address that is not hex digits, `:` and `.` inside `[` and `]`"
            }
            HostPortWhy::NoPort => "has no `:port` after its host",
            HostPortWhy::BadPort => "has a port that is not a number from 1 to 65535",
        }
    }
}

impl fmt::Display for HostPortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host and port {:?} {}", self.given, self.why.says())
    }
}

impl core::error::Error for HostPortError {}

/// Why `host` is no host, if it is not: a DNS name, an IPv4 address (which is one by these
/// rules too), or an IPv6 address in brackets. A `const fn`, shared by run time and bstd's
/// `host!`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn refused_host(host: &str) -> Option<HostPortWhy> {
    let bytes = host.as_bytes();
    if bytes.is_empty() {
        return Some(HostPortWhy::EmptyHost);
    }
    if bytes.len() > HOST_MOST {
        return Some(HostPortWhy::HostTooLong);
    }
    if bytes[0] == b'[' {
        if bytes.len() < 3 || bytes[bytes.len() - 1] != b']' {
            return Some(HostPortWhy::BadBrackets);
        }
        let mut at = 1;
        while at < bytes.len() - 1 {
            let byte = bytes[at];
            if !(byte.is_ascii_hexdigit() || byte == b':' || byte == b'.') {
                return Some(HostPortWhy::BadBrackets);
            }
            at += 1;
        }
        return None;
    }
    // Each `.`-separated label, from `start` to the dot (or the end) at `at`.
    let (mut start, mut at) = (0, 0);
    while at <= bytes.len() {
        if at == bytes.len() || bytes[at] == b'.' {
            let len = at - start;
            if len == 0 || len > 63 || bytes[start] == b'-' || bytes[at - 1] == b'-' {
                return Some(HostPortWhy::BadLabel);
            }
            start = at + 1;
        } else if !(bytes[at].is_ascii_alphanumeric() || bytes[at] == b'-') {
            return Some(HostPortWhy::BadByte);
        }
        at += 1;
    }
    None
}

/// Where `raw` (`host:port`) splits: the index of the `:` before the port, or why there is
/// none. The port is after the last `:`, so `[::1]:443` splits after its `]`.
const fn split(raw: &str) -> Result<usize, HostPortWhy> {
    let bytes = raw.as_bytes();
    let mut at = bytes.len();
    while at > 0 {
        at -= 1;
        match bytes[at] {
            b':' => {
                // A `:` inside brackets is the address's, not the port's.
                if bytes[0] == b'[' && (at == 0 || bytes[at - 1] != b']') {
                    return Err(HostPortWhy::NoPort);
                }
                return Ok(at);
            }
            b']' => return Err(HostPortWhy::NoPort),
            _ => {}
        }
    }
    Err(HostPortWhy::NoPort)
}

/// The port `digits` spell, from 1 to 65535.
const fn port_of(digits: &[u8]) -> Result<u16, HostPortWhy> {
    if digits.is_empty() || digits.len() > 5 {
        return Err(HostPortWhy::BadPort);
    }
    let (mut port, mut at) = (0u32, 0);
    while at < digits.len() {
        if !digits[at].is_ascii_digit() {
            return Err(HostPortWhy::BadPort);
        }
        port = port * 10 + (digits[at] - b'0') as u32;
        at += 1;
    }
    if port == 0 || port > u16::MAX as u32 {
        return Err(HostPortWhy::BadPort);
    }
    Ok(port as u16)
}

/// Why `raw` (`host:port`) is none, if it is not: [`HostPort::parse`]'s rule, as a `const fn`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn refused(raw: &str) -> Option<HostPortWhy> {
    let colon = match split(raw) {
        Ok(colon) => colon,
        Err(why) => return Some(why),
    };
    let (host, port) = raw.as_bytes().split_at(colon);
    let host = match core::str::from_utf8(host) {
        Ok(host) => host,
        Err(_) => return Some(HostPortWhy::BadByte),
    };
    if let Some(why) = refused_host(host) {
        return Some(why);
    }
    let (_, digits) = port.split_at(1);
    match port_of(digits) {
        Ok(_) => None,
        Err(why) => Some(why),
    }
}

/// [`refused`], as a compile-time panic: what bstd's `host!` evaluates in a `const`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn check_literal(raw: &str) {
    if let Some(why) = refused(raw) {
        panic!("{}", why.says());
    }
}

impl HostPort {
    /// `host` and `port`, checked.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(host: &str, port: u16) -> Result<HostPort, HostPortError> {
        let why = match refused_host(host) {
            Some(why) => Some(why),
            None if port == 0 => Some(HostPortWhy::BadPort),
            None => None,
        };
        if let Some(why) = why {
            // alloc: an error's message.
            return Err(HostPortError { given: format!("{host}:{port}"), why });
        }
        // alloc: once per name, its host.
        Ok(HostPort { host: Arc::from(host), port })
    }

    /// `host:port`, checked: `api.stripe.com:443`, `127.0.0.1:8080`, `[::1]:443`.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn parse(raw: &str) -> Result<HostPort, HostPortError> {
        // alloc: an error's message.
        let refuse = |why| HostPortError { given: raw.to_owned(), why };
        if let Some(why) = refused(raw) {
            return Err(refuse(why));
        }
        let colon = split(raw).map_err(refuse)?;
        let port = port_of(&raw.as_bytes()[colon + 1..]).map_err(refuse)?;
        // alloc: once per name, its host.
        Ok(HostPort { host: Arc::from(&raw[..colon]), port })
    }

    /// The host, an IPv6 address with its brackets.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The socket address it names when its host is an IP address; `None` for a DNS name,
    /// which needs a lookup.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        let host = self.host();
        let host = host.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(host);
        let ip: core::net::IpAddr = host.parse().ok()?;
        Some(SocketAddr::new(ip, self.port))
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl fmt::Debug for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostPort({}:{})", self.host, self.port)
    }
}

impl TryFrom<&str> for HostPort {
    type Error = HostPortError;

    #[blaze::pure]
    #[blaze::nonblocking]
    fn try_from(raw: &str) -> Result<HostPort, HostPortError> {
        HostPort::parse(raw)
    }
}
