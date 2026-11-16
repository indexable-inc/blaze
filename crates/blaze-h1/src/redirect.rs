//! The redirect policy: which `Location`s are followed, and how many. By default a redirect
//! stays on its scheme (never `https` to `http`), may change host (crates.io sends downloads to
//! its CDN, GitHub sends archives to `codeload`), and is followed at most a fixed number of
//! times, so a loop is a typed refusal ([`Why::RedirectsOver`]), not a hang.
//!
//! A `Location` is followed when it is an absolute URL, a scheme-relative `//authority/path`
//! or an absolute path `/path`; a relative path (`../x`) is refused ([`Why::Location`]): no
//! server blaze fetches from sends one, and resolving one needs the base's path, which a
//! redirect's target should not lean on. The fragment is dropped.

#![blaze::sans_io]

use crate::error::Why;

/// The policy: every field is the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Redirects {
    /// The most redirects followed for one request.
    pub most: u8,
    /// Whether a redirect may change the scheme.
    pub cross_scheme: bool,
    /// Whether a redirect may change the host or port.
    pub cross_origin: bool,
}

/// A scheme and an authority (`host` or `host:port`), as the request was sent to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin<'a> {
    pub scheme: &'a str,
    pub authority: &'a str,
}

/// Where a redirect goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hop<'a> {
    pub origin: Origin<'a>,
    /// The path and query, `/` at least.
    pub target: &'a str,
}

/// A request's redirects so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Follow {
    policy: Redirects,
    hops: u8,
}

/// Whether `status` is a redirect this client follows.
pub const fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

impl Follow {
    pub const fn new(policy: Redirects) -> Follow {
        Follow { policy, hops: 0 }
    }

    /// Redirects followed so far.
    pub const fn hops(&self) -> u8 {
        self.hops
    }

    /// Where a response of `status` with `location` sends a request sent to `from`: `None` if it
    /// is no redirect, else the next hop, or why it is not followed.
    pub fn next<'a>(&mut self, from: Origin<'a>, status: u16, location: Option<&'a [u8]>) -> Result<Option<Hop<'a>>, Why> {
        if !is_redirect(status) {
            return Ok(None);
        }
        let location = location.ok_or(Why::NoLocation)?;
        if self.hops >= self.policy.most {
            return Err(Why::RedirectsOver { most: self.policy.most });
        }
        let hop = parse(from, location)?;
        if !hop.origin.scheme.eq_ignore_ascii_case(from.scheme) && !self.policy.cross_scheme {
            return Err(Why::RedirectScheme);
        }
        if !same_authority(hop.origin, from) && !self.policy.cross_origin {
            return Err(Why::RedirectOrigin);
        }
        self.hops += 1;
        Ok(Some(hop))
    }
}

fn parse<'a>(from: Origin<'a>, location: &'a [u8]) -> Result<Hop<'a>, Why> {
    let text = core::str::from_utf8(location).map_err(|_| Why::Location)?;
    if text.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(Why::Location);
    }
    let text = text.split('#').next().unwrap_or("");
    let (scheme, rest) = match text.find("://") {
        Some(i) if is_scheme(&text[..i]) => (&text[..i], &text[i + 3..]),
        _ => match text.strip_prefix("//") {
            Some(rest) => (from.scheme, rest),
            None if text.starts_with('/') => return Ok(Hop { origin: from, target: text }),
            None => return Err(Why::Location),
        },
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') {
        return Err(Why::Location);
    }
    let target = match &rest[end..] {
        "" => "/",
        t if t.starts_with('?') => return Err(Why::Location),
        t => t,
    };
    Ok(Hop { origin: Origin { scheme, authority }, target })
}

fn is_scheme(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
}

/// Whether two authorities name one host and port, each scheme's default port written or not.
fn same_authority(a: Origin<'_>, b: Origin<'_>) -> bool {
    fn bare(o: Origin<'_>) -> &str {
        let default = match () {
            () if o.scheme.eq_ignore_ascii_case("http") => ":80",
            () if o.scheme.eq_ignore_ascii_case("https") => ":443",
            () => return o.authority,
        };
        o.authority.strip_suffix(default).unwrap_or(o.authority)
    }
    bare(a).eq_ignore_ascii_case(bare(b))
}
