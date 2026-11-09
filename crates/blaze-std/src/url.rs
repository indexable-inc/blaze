//! Addresses as types (docs/v2/plan/design/std.md, "Names are typed, and authority is
//! stored"): a [`Scheme`], an [`Origin`] (a scheme, a host and a port: what
//! [`crate::requests::Http::only`] narrows to) and a [`Url`], each checked as it is made. A `Url`
//! of any scheme is what [`crate::opener::Opener`] hands to the user's default application; one
//! with an authority also has its [`Url::origin`] and its [`Url::target`], which is what an
//! HTTP request needs.

use std::{fmt, sync::Arc};

use crate::net::HostPort;

/// A URL's scheme, in lower case: a letter, then letters, digits, `+`, `-` and `.`
/// (RFC 3986 section 3.1).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount
pub struct Scheme(Arc<str>);

/// What [`Url::parse`] or [`Scheme::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct UrlError {
    pub url: String,
    pub why: UrlWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlWhy {
    /// No `:` after a scheme.
    NoScheme,
    /// The scheme is empty, does not start with a letter, or holds a byte that is no letter,
    /// digit, `+`, `-` or `.`.
    BadScheme,
    /// Nothing after the scheme's `:`.
    Empty,
    /// A space or a control byte.
    Forbidden,
    /// A user or password before the host (`user@host`): a secret does not ride in a name.
    UserInfo,
    /// The host and port after `//` are no [`HostPort`].
    BadAuthority,
}

impl UrlWhy {
    const fn says(self) -> &'static str {
        match self {
            UrlWhy::NoScheme => "has no `scheme:`",
            UrlWhy::BadScheme => {
                "has a scheme that is not a letter then letters, digits, `+`, `-` or `.`"
            }
            UrlWhy::Empty => "has nothing after its scheme",
            UrlWhy::Forbidden => "holds a space or a control byte",
            UrlWhy::UserInfo => "names a user before its host: a secret does not ride in a URL",
            UrlWhy::BadAuthority => "has a host or port after `//` that is none",
        }
    }
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "URL {:?} {}", self.url, self.why.says())
    }
}

impl std::error::Error for UrlError {}

/// Whether `raw` is a scheme as written, in either case.
fn is_scheme(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let Some(first) = bytes.first() else { return false };
    first.is_ascii_alphabetic()
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

impl Scheme {
    /// `raw`, checked and put in lower case.
    pub fn new(raw: &str) -> Result<Scheme, UrlError> {
        if !is_scheme(raw) {
            // alloc: an error's message.
            return Err(UrlError { url: raw.to_owned(), why: UrlWhy::BadScheme });
        }
        // alloc: once per name, its text in lower case.
        Ok(Scheme(Arc::from(raw.to_ascii_lowercase())))
    }

    /// `http`.
    pub fn http() -> Scheme {
        // alloc: once per name, its text.
        Scheme(Arc::from("http"))
    }

    /// `https`.
    pub fn https() -> Scheme {
        // alloc: once per name, its text.
        Scheme(Arc::from("https"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The port a URL of this scheme means when it names none: 80 for `http`, 443 for `https`.
    pub fn default_port(&self) -> Option<u16> {
        match self.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Scheme({:?})", self.as_str())
    }
}

/// A scheme, a host and a port: whom a request is made to.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)] // clone: refcounts
pub struct Origin {
    scheme: Scheme,
    at: HostPort,
}

impl Origin {
    pub fn new(scheme: Scheme, at: HostPort) -> Origin {
        Origin { scheme, at }
    }

    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    pub fn at(&self) -> &HostPort {
        &self.at
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.at)
    }
}

/// A URL: a scheme and what follows it. One with an authority (`scheme://host[:port]/..`)
/// also has an [`Origin`] and a target (its path and query).
#[derive(Clone, PartialEq, Eq, Hash)] // clone: refcounts
pub struct Url {
    raw: Arc<str>,
    scheme: Scheme,
    origin: Option<Origin>,
    /// The path and query, `/` when there is none; for a URL with no authority, what follows
    /// the scheme.
    target: Arc<str>,
}

impl Url {
    /// `raw`, checked: `https://api.stripe.com/v1/charges?limit=3`, `mailto:a@b.example`.
    pub fn parse(raw: &str) -> Result<Url, UrlError> {
        // alloc: an error's message.
        let refuse = |why| UrlError { url: raw.to_owned(), why };
        let Some(colon) = raw.find(':') else { return Err(refuse(UrlWhy::NoScheme)) };
        if !is_scheme(&raw[..colon]) {
            return Err(refuse(UrlWhy::BadScheme));
        }
        // alloc: once per name, its scheme in lower case.
        let scheme = Scheme(Arc::from(raw[..colon].to_ascii_lowercase()));
        let rest = &raw[colon + 1..];
        if rest.is_empty() {
            return Err(refuse(UrlWhy::Empty));
        }
        if rest.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
            return Err(refuse(UrlWhy::Forbidden));
        }
        let Some(after) = rest.strip_prefix("//") else {
            // alloc: once per name, its text and its target.
            return Ok(Url { raw: Arc::from(raw), scheme, origin: None, target: Arc::from(rest) });
        };
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let (authority, tail) = after.split_at(end);
        if authority.contains('@') {
            return Err(refuse(UrlWhy::UserInfo));
        }
        let origin = if authority.is_empty() {
            None
        } else {
            let at = match (HostPort::parse(authority), scheme.default_port()) {
                (Ok(at), _) => at,
                (Err(_), Some(port)) => {
                    HostPort::new(authority, port).map_err(|_| refuse(UrlWhy::BadAuthority))?
                }
                (Err(_), None) => return Err(refuse(UrlWhy::BadAuthority)),
            };
            // clone: the origin's scheme, a refcount.
            Some(Origin { scheme: scheme.clone(), at })
        };
        let tail = tail.split('#').next().unwrap_or("");
        let target: Arc<str> = match tail.as_bytes().first() {
            // alloc: once per name, its target.
            Some(b'/') => Arc::from(tail),
            // alloc: once per name, its target.
            Some(_) => Arc::from(format!("/{tail}")),
            // alloc: once per name, its target.
            None => Arc::from("/"),
        };
        // alloc: once per name, its text.
        Ok(Url { raw: Arc::from(raw), scheme, origin, target })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    /// Whom it is asked of, when it has an authority.
    pub fn origin(&self) -> Option<&Origin> {
        self.origin.as_ref()
    }

    /// Its path and query, `/` when it has none.
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Url({:?})", self.as_str())
    }
}

impl TryFrom<&str> for Url {
    type Error = UrlError;

    fn try_from(raw: &str) -> Result<Url, UrlError> {
        Url::parse(raw)
    }
}
