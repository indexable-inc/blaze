//! A connection's bytes through blaze-api's HTTP request parser (`http::RequestParser`, what
//! the daemon's HTTP side and CI's webhook cut requests with), pushed all at once and a byte
//! at a time, with the daemon's bound on a body and a small one; each request it gives is
//! written again with `http::request` and parsed back.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - However the bytes are pushed, the parser gives the same requests and then the same
//!   error, or waits; once it has failed every `next` gives that error again.
//! - Parsing asks for at most `blaze_fuzz::allowed(len)` bytes at once.
//! - A request has a method that is a token, a target without spaces, header names that are
//!   tokens, values with no CR, LF or space or tab at their ends, and a body no longer than
//!   the bound.
//! - A request written by `http::request` from what was parsed (its own Host and
//!   Content-Length in place of any it had) parses back to that request, alone.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_api::{
    LONGEST_REQUEST,
    http::{self, HttpError, HttpRequest, RequestParser},
};
use blaze_fuzzer::Target;

const TARGET: Target = Target {
    name: "api_http_request",
    check,
    seeds,
    dictionary: &[
        b"\r\n",
        b"\r\n\r\n",
        b"POST /api HTTP/1.1\r\n",
        b"GET / HTTP/1.0\r\n",
        b"HTTP/2",
        b"Content-Length: ",
        b"content-length:0\r\n",
        b"Transfer-Encoding: chunked\r\n",
        b"Transfer-Encoding: gzip\r\n",
        b"Content-Type: application/cbor\r\n",
        b"Authorization: Bearer ",
        b"Connection: close\r\n",
        b"0\r\n\r\n",
        b"ffffffffffffffff\r\n",
        b"10000000000000000\r\n",
        b";ext=1",
        b" \t",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

/// The bound on a body small enough for the fuzzer to cross.
const SMALL_BODY: usize = 64;
/// The longest line of a head, without its CRLF ("a head line over 8 KiB" is refused).
const LONGEST_LINE: usize = 8 << 10;
/// The most header lines a head may have.
const MOST_HEADERS: usize = 100;

/// Every request the parser has, then its error if it failed.
fn drain(parser: &mut RequestParser, requests: &mut Vec<HttpRequest>) -> Option<HttpError> {
    loop {
        match parser.next() {
            Ok(Some(request)) => requests.push(request),
            Ok(None) => return None,
            Err(error) => {
                assert_eq!(parser.next(), Err(error.clone()), "a failed parser gives its error again");
                return Some(error);
            }
        }
    }
}

fn parse_whole(data: &[u8], most_body: usize) -> (Vec<HttpRequest>, Option<HttpError>) {
    let mut parser = RequestParser::new(most_body);
    parser.push(data);
    let mut requests = Vec::new();
    let error = drain(&mut parser, &mut requests);
    (requests, error)
}

fn parse_bytewise(data: &[u8], most_body: usize) -> (Vec<HttpRequest>, Option<HttpError>) {
    let mut parser = RequestParser::new(most_body);
    let mut requests = Vec::new();
    for at in 0..data.len() {
        parser.push(&data[at..at + 1]);
        if let Some(error) = drain(&mut parser, &mut requests) {
            parser.push(&data[at + 1..]);
            assert_eq!(parser.next(), Err(error.clone()), "a failed parser stays failed past a push");
            return (requests, Some(error));
        }
    }
    (requests, None)
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn well_formed(request: &HttpRequest, most_body: usize) {
    assert!(
        !request.method.is_empty() && request.method.bytes().all(is_tchar),
        "a method is a token: {:?}",
        request.method
    );
    assert!(
        !request.path.is_empty() && !request.path.contains([' ', '\r', '\n']),
        "a target has no spaces: {:?}",
        request.path
    );
    for (name, value) in &request.headers {
        assert!(!name.is_empty() && name.bytes().all(is_tchar), "a header name is a token: {name:?}");
        assert!(!value.contains(['\r', '\n']), "a header value is on one line: {value:?}");
        assert!(
            !value.starts_with([' ', '\t']) && !value.ends_with([' ', '\t']),
            "a header value is trimmed: {value:?}"
        );
    }
    assert!(request.body.len() <= most_body, "a body of {} bytes past the bound of {most_body}", request.body.len());
}

/// `request` written again by `http::request` parses back to it, alone.
fn written_again(request: &HttpRequest, most_body: usize) {
    let framing = ["host", "content-length", "transfer-encoding"];
    let kept: Vec<(&str, &str)> = request
        .headers
        .iter()
        .filter(|(name, _)| !framing.iter().any(|it| name.eq_ignore_ascii_case(it)))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let start = request.method.len() + 1 + request.path.len() + " HTTP/1.1".len();
    let longest = kept.iter().map(|(name, value)| name.len() + 2 + value.len()).max().unwrap_or(0);
    if kept.len() + 2 > MOST_HEADERS || start.max(longest) > LONGEST_LINE {
        return;
    }
    let bytes = http::request(&request.method, "fuzz", &request.path, &kept, &request.body);
    let (again, error) = parse_whole(&bytes, most_body);
    assert_eq!(error, None, "a request written again parses");
    let length = request.body.len().to_string();
    let mut headers = vec![("Host".to_owned(), "fuzz".to_owned())];
    headers.extend(kept.iter().map(|&(name, value)| (name.to_owned(), value.to_owned())));
    headers.push(("Content-Length".to_owned(), length));
    let expected = HttpRequest {
        method: request.method.clone(),
        path: request.path.clone(),
        headers,
        body: request.body.clone(),
    };
    assert_eq!(again, [expected], "a request written again parses back to itself");
}

/// The bounds on a body, one a run (`blaze_fuzzer::case`); the plain test takes both of each seed.
const BODIES: [usize; 2] = [LONGEST_REQUEST as usize, SMALL_BODY];

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, BODIES.len()));
}

fn check_case(data: &[u8], case: usize) {
    let most_body = BODIES[case];
    let (most, (requests, error)) = blaze_fuzz::largest_allocation(|| parse_whole(data, most_body));
    assert!(most <= blaze_fuzz::allowed(data.len()), "parsing {} bytes asked for {most} at once", data.len());
    let (bytewise, bytewise_error) = parse_bytewise(data, most_body);
    assert_eq!(bytewise, requests, "a byte at a time the parser gives the same requests");
    assert_eq!(bytewise_error, error, "a byte at a time the parser fails alike");
    for request in &requests {
        well_formed(request, most_body);
        written_again(request, most_body);
    }
}

/// Requests as the client and GitHub send them, one after another on a connection.
fn seeds() -> Vec<Vec<u8>> {
    let body = [0xa3, 0x62, b'i', b'd', 0x01];
    let api = http::request(
        "POST",
        "127.0.0.1:7777",
        "/api",
        &[("Content-Type", "application/cbor"), ("Authorization", "Bearer t")],
        &body,
    );
    let mut chunked = b"POST /api HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    http::chunk_into(&mut chunked, &body);
    http::chunk_into(&mut chunked, b"more");
    chunked.extend_from_slice(b"0\r\nX-Trailer: 1\r\n\r\n");
    let mut pipelined = api.clone();
    pipelined.extend_from_slice(&chunked);
    pipelined.extend_from_slice(b"\r\nGET /status HTTP/1.0\r\nConnection: close\r\n\r\n");
    let webhook = http::request(
        "POST",
        "ci",
        "/webhook",
        &[("X-GitHub-Event", "push"), ("Content-Type", "application/json")],
        b"{\"ref\":\"refs/heads/main\"}",
    );
    let too_big = http::request("POST", "h", "/api", &[], &[0; SMALL_BODY + 1]);
    vec![Vec::new(), api, chunked, pipelined, webhook, too_big]
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::BODIES.len(), super::check_case);
    }
}
