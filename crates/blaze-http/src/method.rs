//! A request's method: the eight of RFC 9110 section 9.3 a client asks with, `CONNECT` aside
//! (a proxy tunnel's, written by [`crate::proxy`] itself).

#![blaze::sans_io]

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Trace,
}

impl Method {
    /// Its name on the wire, in upper case.
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
        }
    }

    /// Whether sending it twice does what sending it once does (RFC 9110 section 9.2.2): what
    /// may be asked again on a new connection, or sent as replayable early data.
    pub const fn idempotent(self) -> bool {
        !matches!(self, Method::Post | Method::Patch)
    }

    /// Whether a request of it carries a body, so an empty one is still said
    /// (`Content-Length: 0`).
    pub const fn carries_body(self) -> bool {
        matches!(self, Method::Post | Method::Put | Method::Patch)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
