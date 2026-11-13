//! Addresses as types (docs/v2/plan/design/std.md, "Names are typed, and authority is
//! stored"): a [`Scheme`], an [`Origin`] (a scheme, a host and a port: what
//! [`crate::requests::Http::only`] narrows to) and a [`Url`], each checked as it is made. A `Url`
//! of any scheme is what [`crate::opener::Opener`] hands to the user's default application; one
//! with an authority also has its [`Url::origin`] and its [`Url::target`], which is what an
//! HTTP request needs.

use alloc::{alloc::Global, string::String, sync::Arc};
use core::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

use crate::{
    mem::{self as bmem, OutOfMemory, heap},
    name::Excerpt,
    net::{HostPort, HostPortWhy},
};

/// Text a name holds: a literal the crate was built with, or text kept once on the global
/// heap and shared by its clones. Compared, ordered and hashed as its text, whichever it is.
#[derive(Clone)] // clone: a copy or a refcount
enum Text {
    Static(&'static str),
    // alloc: once per name, its text.
    Kept(Arc<String>),
}

impl Text {
    /// `text` kept on the global heap, or the error saying it had no room.
    fn kept(text: String) -> Result<Text, UrlWhy> {
        heap::shared(text).map(Text::Kept).map_err(UrlWhy::OutOfMemory)
    }

    /// `text` copied onto the global heap and kept.
    fn copied(text: &str) -> Result<Text, UrlWhy> {
        // alloc: once per name, its text.
        Text::kept(heap::string(text).map_err(UrlWhy::OutOfMemory)?)
    }

    fn as_str(&self) -> &str {
        match self {
            Text::Static(text) => text,
            Text::Kept(text) => text.as_str(),
        }
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Text {}

impl PartialOrd for Text {
    fn partial_cmp(&self, other: &Text) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Text {
    fn cmp(&self, other: &Text) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for Text {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

/// A URL's scheme, in lower case: a letter, then letters, digits, `+`, `-` and `.`
/// (RFC 3986 section 3.1).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a copy or a refcount
pub struct Scheme(Text);

/// What [`Url::parse`] or [`Scheme::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct UrlError {
    /// The URL refused, or its start when it is long: kept in the error, which allocates
    /// nothing.
    pub url: Excerpt,
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
    /// It passed, and the global heap had no room to keep it.
    OutOfMemory(OutOfMemory),
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
            UrlWhy::OutOfMemory(_) => "could not be kept: the global heap had no room",
        }
    }
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "URL {:?} {}", self.url, self.why.says())?;
        match self.why {
            UrlWhy::OutOfMemory(error) => write!(f, " ({error})"),
            _ => Ok(()),
        }
    }
}

impl core::error::Error for UrlError {}

/// The error of `raw` refused for `why`.
fn refused(raw: &str, why: UrlWhy) -> UrlError {
    UrlError { url: Excerpt::of(raw), why }
}

/// Whether `raw` is a scheme as written, in either case.
fn is_scheme(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let Some(first) = bytes.first() else { return false };
    first.is_ascii_alphabetic()
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

/// `raw`, a scheme as [`is_scheme`] checked it, kept in lower case: `http` and `https` as the
/// literals they are, anything else on the global heap.
fn lowered(raw: &str) -> Result<Scheme, UrlWhy> {
    if raw.eq_ignore_ascii_case("http") {
        return Ok(Scheme::http());
    }
    if raw.eq_ignore_ascii_case("https") {
        return Ok(Scheme::https());
    }
    // alloc: once per name, its text in lower case.
    let mut lower = heap::string(raw).map_err(UrlWhy::OutOfMemory)?;
    lower.make_ascii_lowercase();
    Ok(Scheme(Text::kept(lower)?))
}

impl Scheme {
    /// `raw`, checked and put in lower case.
    pub fn new(raw: &str) -> Result<Scheme, UrlError> {
        if !is_scheme(raw) {
            return Err(refused(raw, UrlWhy::BadScheme));
        }
        lowered(raw).map_err(|why| refused(raw, why))
    }

    /// `http`.
    pub fn http() -> Scheme {
        Scheme(Text::Static("http"))
    }

    /// `https`.
    pub fn https() -> Scheme {
        Scheme(Text::Static("https"))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
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
    raw: Text,
    scheme: Scheme,
    origin: Option<Origin>,
    /// The path and query, `/` when there is none; for a URL with no authority, what follows
    /// the scheme.
    target: Text,
}

/// What a host and port that [`HostPort`] refused is to a URL: the global heap's refusal is
/// that still, anything else a bad authority.
fn authority_why(why: HostPortWhy) -> UrlWhy {
    match why {
        HostPortWhy::OutOfMemory(error) => UrlWhy::OutOfMemory(error),
        _ => UrlWhy::BadAuthority,
    }
}

impl Url {
    /// `raw`, checked: `https://api.stripe.com/v1/charges?limit=3`, `mailto:a@b.example`.
    pub fn parse(raw: &str) -> Result<Url, UrlError> {
        let refuse = |why| refused(raw, why);
        let Some((scheme, rest)) = raw.split_once(':') else { return Err(refuse(UrlWhy::NoScheme)) };
        if !is_scheme(scheme) {
            return Err(refuse(UrlWhy::BadScheme));
        }
        let scheme = lowered(scheme).map_err(refuse)?;
        if rest.is_empty() {
            return Err(refuse(UrlWhy::Empty));
        }
        if rest.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
            return Err(refuse(UrlWhy::Forbidden));
        }
        let Some(after) = rest.strip_prefix("//") else {
            // alloc: once per name, its text and its target.
            let (raw, target) = (Text::copied(raw).map_err(refuse)?, Text::copied(rest).map_err(refuse)?);
            return Ok(Url { raw, scheme, origin: None, target });
        };
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        // `end` is where `find` stopped, or the end: a boundary of `after` either way.
        let Some((authority, tail)) = after.split_at_checked(end) else { return Err(refuse(UrlWhy::BadAuthority)) };
        if authority.contains('@') {
            return Err(refuse(UrlWhy::UserInfo));
        }
        let origin = if authority.is_empty() {
            None
        } else {
            let at = match (HostPort::parse(authority), scheme.default_port()) {
                (Ok(at), _) => at,
                (Err(error), _) if matches!(error.why, HostPortWhy::OutOfMemory(_)) => {
                    return Err(refuse(authority_why(error.why)));
                }
                (Err(_), Some(port)) => HostPort::new(authority, port).map_err(|error| refuse(authority_why(error.why)))?,
                (Err(_), None) => return Err(refuse(UrlWhy::BadAuthority)),
            };
            // clone: the origin's scheme, a copy or a refcount.
            Some(Origin { scheme: scheme.clone(), at })
        };
        let tail = tail.split('#').next().unwrap_or("");
        let target = match tail.as_bytes().first() {
            // alloc: once per name, its target.
            Some(b'/') => Text::copied(tail).map_err(refuse)?,
            Some(_) => {
                // alloc: once per name, its target, as a std `String` its handle shares.
                let mut target = bmem::String::new_in(Global);
                target.push('/').map_err(|full| refuse(UrlWhy::OutOfMemory(full)))?;
                target.push_str(tail).map_err(|full| refuse(UrlWhy::OutOfMemory(full)))?;
                Text::kept(target.into_std()).map_err(refuse)?
            }
            None => Text::Static("/"),
        };
        // alloc: once per name, its text.
        Ok(Url { raw: Text::copied(raw).map_err(refuse)?, scheme, origin, target })
    }

    pub fn as_str(&self) -> &str {
        self.raw.as_str()
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
        self.target.as_str()
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
