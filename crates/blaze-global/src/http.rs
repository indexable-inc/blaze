//! The HTTP/1.1 the global cache speaks: `POST` requests with a `Content-Length` body, and
//! responses the same way, any number back to back on one connection (pipelined, answered in
//! order). Nothing else: a chunked body, a request of another method, or a head longer than
//! [`HEAD_MOST`] is refused, saying so. A head is parsed only once it is whole, and a body's
//! length is checked against its bound before anything waits for it.

/// The most bytes of a head, request line or status line and headers.
pub const HEAD_MOST: usize = 16 << 10;

pub(crate) const CONTENT_TYPE: &str = "application/x-blaze-global";

#[cfg(test)]
pub(crate) fn write_request(host: &str, path: &str, body: &[u8], out: &mut Vec<u8>) {
    write_request_head(host, path, body.len(), out);
    out.extend_from_slice(body);
}

/// The head of a request whose body of `len` bytes follows it.
pub(crate) fn write_request_head(host: &str, path: &str, len: usize, out: &mut Vec<u8>) {
    use std::io::Write as _;
    write!(
        out,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: blaze/{}\r\nContent-Type: {CONTENT_TYPE}\r\nContent-Length: {len}\r\n\r\n",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("writing to a vector");
}

pub(crate) fn write_response(status: u16, body: &[u8], close: bool, out: &mut Vec<u8>) {
    write_response_head(status, body.len(), close, out);
    out.extend_from_slice(body);
}

/// The head of a response whose body of `len` bytes follows it.
pub(crate) fn write_response_head(status: u16, len: usize, close: bool, out: &mut Vec<u8>) {
    use std::io::Write as _;
    let connection = if close { "Connection: close\r\n" } else { "" };
    write!(
        out,
        "HTTP/1.1 {status} {}\r\nContent-Type: {CONTENT_TYPE}\r\n{connection}Content-Length: {len}\r\n\r\n",
        reason_phrase(status),
    )
    .expect("writing to a vector");
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        500 => "Internal Server Error",
        413 => "Content Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// A response's head, once whole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ResponseHead {
    pub status: u16,
    /// The head's length, so its body starts here.
    pub head: usize,
    pub body: usize,
    /// The server closes the connection after this response.
    pub close: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RequestHead {
    pub path: String,
    pub head: usize,
    pub body: usize,
}

/// Why a request is refused, with the status to answer it with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Bad {
    pub status: u16,
    pub what: String,
}

/// The head `buffer` starts with: `None` until it is whole. `body_most` bounds its body.
pub(crate) fn response_head(buffer: &[u8], body_most: u64) -> Result<Option<ResponseHead>, String> {
    let Some((head, lines)) = head_lines(buffer).map_err(|bad| bad.what)? else { return Ok(None) };
    let mut lines = lines.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut words = status_line.splitn(3, ' ');
    if words.next() != Some("HTTP/1.1") {
        // alloc: an error's message, made only when it fails.
        return Err(format!("not an HTTP/1.1 response: {status_line:?}"));
    }
    let status = words
        .next()
        .filter(|it| it.len() == 3)
        .and_then(|it| it.parse::<u16>().ok())
        // alloc: an error's message, made only when it fails.
        .ok_or_else(|| format!("a status line with no status: {status_line:?}"))?;
    let fields = fields(lines, body_most).map_err(|bad| bad.what)?;
    Ok(Some(ResponseHead { status, head, body: fields.body, close: fields.close }))
}

pub(crate) fn request_head(buffer: &[u8], body_most: impl Fn(&str) -> u64) -> Result<Option<RequestHead>, Bad> {
    let Some((head, lines)) = head_lines(buffer)? else { return Ok(None) };
    let mut lines = lines.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut words = request_line.split(' ');
    let (Some(method), Some(path), Some(version), None) = (words.next(), words.next(), words.next(), words.next())
    else {
        // alloc: an error's message, made only when it fails.
        return Err(bad(400, format!("not a request line: {request_line:?}")));
    };
    if version != "HTTP/1.1" {
        // alloc: an error's message, made only when it fails.
        return Err(bad(400, format!("not an HTTP/1.1 request: {request_line:?}")));
    }
    if method != "POST" {
        // alloc: an error's message, made only when it fails.
        return Err(bad(405, format!("{method} {path}: the global cache takes POST only")));
    }
    let fields = fields(lines, body_most(path))?;
    // alloc: the edge: the request's path, one a request.
    Ok(Some(RequestHead { path: path.to_owned(), head, body: fields.body }))
}

fn bad(status: u16, what: String) -> Bad {
    Bad { status, what }
}

/// The head's length and its text, without the blank line that ends it.
fn head_lines(buffer: &[u8]) -> Result<Option<(usize, &str)>, Bad> {
    let window = &buffer[..buffer.len().min(HEAD_MOST)];
    let Some(end) = memchr::memmem::find(window, b"\r\n\r\n") else {
        if buffer.len() >= HEAD_MOST {
            // alloc: an error's message, made only when it fails.
            return Err(bad(400, format!("a head longer than the {HEAD_MOST} bytes allowed")));
        }
        return Ok(None);
    };
    let text = std::str::from_utf8(&buffer[..end]).map_err(|_| bad(400, "a head that is not UTF-8".into()))?;
    Ok(Some((end + 4, text)))
}

struct Fields {
    body: usize,
    close: bool,
}

fn fields<'a>(lines: impl Iterator<Item = &'a str>, body_most: u64) -> Result<Fields, Bad> {
    let mut body = None;
    let mut close = false;
    for line in lines {
        // alloc: an error's message, made only when it fails.
        let (name, value) = line.split_once(':').ok_or_else(|| bad(400, format!("a header with no colon: {line:?}")))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if body.is_some() {
                return Err(bad(400, "two Content-Length headers".into()));
            }
            if value.is_empty() || !value.bytes().all(|it| it.is_ascii_digit()) {
                // alloc: an error's message, made only when it fails.
                return Err(bad(400, format!("a Content-Length of {value:?}")));
            }
            // alloc: an error's message, made only when it fails.
            let len = value.parse::<u64>().map_err(|_| bad(413, format!("a Content-Length of {value}")))?;
            if len > body_most {
                // alloc: an error's message, made only when it fails.
                return Err(bad(413, format!("a body of {len} bytes, more than the {body_most} allowed")));
            }
            body = Some(len as usize);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // alloc: an error's message, made only when it fails.
            return Err(bad(400, format!("a body sent as {value}: only a Content-Length is taken")));
        } else if name.eq_ignore_ascii_case("connection") {
            close = value.split(',').any(|it| it.trim().eq_ignore_ascii_case("close"));
        }
    }
    let body = body.ok_or_else(|| bad(411, "no Content-Length".into()))?;
    Ok(Fields { body, close })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_and_a_response_round_trip() {
        let mut request = Vec::new();
        write_request("cache.example.com", "/v1/fetch", b"body", &mut request);
        let head = request_head(&request, |_| 10).unwrap().unwrap();
        assert_eq!(head.path, "/v1/fetch");
        assert_eq!(&request[head.head..head.head + head.body], b"body");

        let mut response = Vec::new();
        write_response(200, b"answer", false, &mut response);
        write_response(404, b"", true, &mut response);
        let first = response_head(&response, 10).unwrap().unwrap();
        assert_eq!((first.status, first.body, first.close), (200, 6, false));
        let rest = &response[first.head + first.body..];
        let second = response_head(rest, 10).unwrap().unwrap();
        assert_eq!((second.status, second.body, second.close), (404, 0, true));
    }

    #[test]
    fn a_head_is_read_only_once_whole() {
        let mut response = Vec::new();
        write_response(200, b"x", false, &mut response);
        let end = response.len() - 1;
        for cut in 0..end {
            assert_eq!(response_head(&response[..cut], 10), Ok(None), "{cut}");
        }
        assert!(response_head(&response[..end], 10).unwrap().is_some());
    }

    #[test]
    fn what_is_not_spoken_is_refused() {
        let refused = |text: &str, status: u16, says: &str| {
            let error = request_head(text.as_bytes(), |_| 100).unwrap_err();
            assert_eq!(error.status, status, "{error:?}");
            assert!(error.what.contains(says), "{error:?} does not say {says:?}");
        };
        refused("GET /v1/fetch HTTP/1.1\r\nContent-Length: 0\r\n\r\n", 405, "POST only");
        refused("POST /v1/fetch HTTP/1.0\r\nContent-Length: 0\r\n\r\n", 400, "HTTP/1.1");
        refused("POST /v1/fetch HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 400, "chunked");
        refused("POST /v1/fetch HTTP/1.1\r\nContent-Length: 101\r\n\r\n", 413, "more than");
        refused("POST /v1/fetch HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n", 413, "Content-Length");
        refused("POST /v1/fetch HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 400, "Content-Length");
        refused("POST /v1/fetch HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n", 400, "two");
        refused("POST /v1/fetch HTTP/1.1\r\n\r\n", 411, "no Content-Length");
        refused("POST /v1/fetch HTTP/1.1\r\nbroken\r\n\r\n", 400, "colon");
        let long = vec![b'a'; HEAD_MOST];
        assert!(request_head(&long, |_| 0).unwrap_err().what.contains("longer than"));
        assert!(response_head(b"SPDY/3 200 OK\r\n\r\n", 0).unwrap_err().contains("HTTP/1.1"));
        assert!(response_head(b"HTTP/1.1 OK\r\nContent-Length: 0\r\n\r\n", 0).unwrap_err().contains("no status"));
        assert!(response_head(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n", 10).unwrap_err().contains("more than"));
    }
}
