//! Why a request could not be written or a response was refused (rule 19): the byte of the
//! response it was found at, and what was wrong.

use core::fmt;

use blaze_own::AllocFailure;

/// What was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    // The request.
    /// A method that is no token.
    Method,
    /// A request target that is empty, holds a space or a control byte, or does not start at `/`.
    Target,
    /// A host that is empty or holds a byte no host has.
    Host,
    /// The header at this index has a name that is no token, or is one the writer sets itself
    /// (`Host`, `Content-Length`, `Transfer-Encoding`, `Range`, `Connection`).
    HeaderName { index: u16 },
    /// The header at this index has a value with a CR, LF, NUL or other control byte.
    HeaderValue { index: u16 },
    /// A range whose last byte is before its first.
    RangeOrder { first: u64, last: u64 },

    // The response head.
    /// A head longer than the caller allows.
    HeadOver { most: u32 },
    /// More header lines than the caller allows.
    HeadersOver { most: u16 },
    /// A status line that is no `HTTP/1.x NNN reason`.
    StatusLine,
    /// A protocol switch (101), which this client never asks for.
    Switching,
    /// A header line that is no `name: value` (a space before the colon, a folded line, a
    /// control byte).
    HeaderLine,

    // The response body's framing.
    /// A `Content-Length` that is no number, or two that differ.
    ContentLength,
    /// Both `Content-Length` and `Transfer-Encoding`: a framing two readers may take apart
    /// differently, which is refused, not guessed.
    LengthAndChunked,
    /// A `Transfer-Encoding` other than `chunked` alone.
    TransferEncoding,
    /// A chunk size that is no hex number or overflows.
    ChunkSize,
    /// A chunk-size line longer than the caller allows ([`Limits::most_chunk_line`]).
    ///
    /// [`Limits::most_chunk_line`]: crate::Limits::most_chunk_line
    ChunkLineOver { most: u16 },
    /// A chunk not ended by CRLF.
    ChunkEnd,
    /// A body longer than the caller allows.
    BodyOver { most: u64 },
    /// The connection ended before the body did.
    Truncated { body: u64 },
    /// Bytes after the response's end, on a connection that should have closed.
    AfterEnd,

    // Ranges.
    /// A ranged request answered with a status other than 206 (a 200 is the whole resource:
    /// taking it as the range would be wrong, and fetching the whole is the caller's choice).
    RangeStatus { status: u16 },
    /// No `Content-Range`, or one that is no `bytes first-last/length`.
    ContentRange,
    /// A `Content-Range` that is not the range asked for.
    RangeMismatch { first: u64, last: u64 },

    // Redirects.
    /// More redirects than the policy allows.
    RedirectsOver { most: u8 },
    /// A redirect without a `Location`.
    NoLocation,
    /// A `Location` that is no absolute URL, `//authority/path` or `/path` (relative paths are
    /// not followed), or that names a user.
    Location,
    /// A redirect to another scheme, which the policy refuses (`https` to `http` most of all).
    RedirectScheme,
    /// A redirect to another origin, which the policy refuses.
    RedirectOrigin,

    /// A buffer could not be had.
    Alloc(AllocFailure),
}

/// A refused request or response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    /// The offset in the response's bytes it was found at (0 for a request).
    pub at: u64,
    pub why: Why,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze_h1: at byte {} of the response: ", self.at)?;
        match self.why {
            Why::Method => f.write_str("the method is no token"),
            Why::Target => f.write_str("the request target is no `/path?query`"),
            Why::Host => f.write_str("the host is no host"),
            Why::HeaderName { index } => write!(f, "request header {index} has a name that is no token or is set by the writer"),
            Why::HeaderValue { index } => write!(f, "request header {index} has a control byte in its value"),
            Why::RangeOrder { first, last } => write!(f, "the range {first}-{last} ends before it starts"),
            Why::HeadOver { most } => write!(f, "a head longer than the most allowed, {most} bytes"),
            Why::HeadersOver { most } => write!(f, "more than the most header lines allowed, {most}"),
            Why::StatusLine => f.write_str("a status line that is no `HTTP/1.x NNN reason`"),
            Why::Switching => f.write_str("a protocol switch (101), never asked for"),
            Why::HeaderLine => f.write_str("a header line that is no `name: value`"),
            Why::ContentLength => f.write_str("a Content-Length that is no number, or two that differ"),
            Why::LengthAndChunked => f.write_str("both Content-Length and Transfer-Encoding"),
            Why::TransferEncoding => f.write_str("a Transfer-Encoding other than `chunked`"),
            Why::ChunkSize => f.write_str("a chunk size that is no hex number"),
            Why::ChunkLineOver { most } => write!(f, "a chunk-size line past the most allowed, {most} bytes"),
            Why::ChunkEnd => f.write_str("a chunk not ended by CRLF"),
            Why::BodyOver { most } => write!(f, "a body longer than the most allowed, {most} bytes"),
            Why::Truncated { body } => write!(f, "the connection ended after {body} bytes of the body, before its end"),
            Why::AfterEnd => f.write_str("bytes after the response's end"),
            Why::RangeStatus { status } => write!(f, "a ranged request answered {status}, not 206"),
            Why::ContentRange => f.write_str("no Content-Range of the form `bytes first-last/length`"),
            Why::RangeMismatch { first, last } => write!(f, "the range {first}-{last}, not the one asked for"),
            Why::RedirectsOver { most } => write!(f, "more than {most} redirects"),
            Why::NoLocation => f.write_str("a redirect with no Location"),
            Why::Location => f.write_str("a Location that is no absolute URL or `/path`"),
            Why::RedirectScheme => f.write_str("a redirect to another scheme"),
            Why::RedirectOrigin => f.write_str("a redirect to another origin"),
            Why::Alloc(failure) => write!(f, "{failure}"),
        }
    }
}

impl core::error::Error for Error {}
