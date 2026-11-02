//! A connection's bytes through blaze-api's HTTP response parser (`http::ResponseParser`, what
//! the client reads the daemon's HTTP answers with and CI reads GitHub's), pushed all at once
//! and a byte at a time and then ended, and through `http::read_response`, with no bound on a
//! body as the client has and a small one.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - However the bytes are pushed, the parser gives the same head and the same error, the
//!   same body when it does not fail, and is finished alike; once it has failed every push
//!   gives that error again.
//! - Parsing asks for at most `blaze_fuzz::allowed(len)` bytes at once.
//! - Once the stream has ended, the parser has failed or finished.
//! - A status is three digits, header names are tokens, and a body is no longer than the
//!   bound.
//! - `read_response` gives the head and body the parser does when it finishes, and when it
//!   fails an `InvalidData` error carrying the parser's.

#![forbid(unsafe_code)]

use std::{io, time::Duration};

use blaze_api::http::{self, HttpError, ResponseParser};
use blaze_fuzzer::Target;

const TARGET: Target = Target {
    name: "api_http_response",
    check,
    seeds,
    dictionary: &[
        b"\r\n",
        b"\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n",
        b"HTTP/1.0 100 Continue\r\n\r\n",
        b"HTTP/1.1 101 \r\n",
        b"HTTP/1.1 204 No Content\r\n",
        b"HTTP/1.1 304 \r\n",
        b"Content-Length: ",
        b"Transfer-Encoding: chunked\r\n",
        b"0\r\n\r\n",
        b"ffffffffffffffff\r\n",
        b";ext=1",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// The bound on a body small enough for the fuzzer to cross.
const SMALL_BODY: usize = 64;

/// What a parser made of a stream: its head, the body it gave, whether it finished, and
/// its error.
#[derive(Debug, PartialEq)]
struct Parsed {
    head: Option<(u16, Vec<(String, String)>)>,
    body: Vec<u8>,
    finished: bool,
    error: Option<HttpError>,
}

/// `data` pushed in pieces of at most `piece` bytes, then the end of the stream.
fn parse(data: &[u8], most_body: usize, piece: usize) -> Parsed {
    let mut parser = ResponseParser::new(most_body);
    let mut body = Vec::new();
    let mut error = None;
    for bytes in data.chunks(piece).chain([&[][..]]) {
        let pushed = parser.push(bytes);
        body.extend(parser.take_body());
        if let Err(failed) = pushed {
            assert_eq!(parser.push(b"x"), Err(failed.clone()), "a failed parser gives its error again");
            error = Some(failed);
            // How much of a body over the bound was handed out before the parser saw it was
            // over depends on how its bytes came: a failed response's body is not compared.
            body.clear();
            break;
        }
    }
    let head = parser.head().map(|(status, headers)| (status, headers.to_vec()));
    Parsed { head, body, finished: parser.finished(), error }
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The bounds on a body, one a run (`blaze_fuzzer::case`); the plain test takes both of each seed.
const BODIES: [usize; 2] = [usize::MAX, SMALL_BODY];

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, BODIES.len()));
}

fn check_case(data: &[u8], case: usize) {
    let most_body = BODIES[case];
    let (most, whole) = blaze_fuzz::largest_allocation(|| parse(data, most_body, data.len().max(1)));
    assert!(most <= blaze_fuzz::allowed(data.len()), "parsing {} bytes asked for {most} at once", data.len());
    assert_eq!(parse(data, most_body, 1), whole, "a byte at a time the parser reads the same response");
    assert!(whole.finished || whole.error.is_some(), "an ended stream is a response or an error: {whole:?}");
    if let Some((status, headers)) = &whole.head {
        assert!(*status <= 999, "a status of three digits: {status}");
        for (name, _) in headers {
            assert!(!name.is_empty() && name.bytes().all(is_tchar), "a header name is a token: {name:?}");
        }
    }
    assert!(whole.body.len() <= most_body, "a body of {} bytes past the bound of {most_body}", whole.body.len());

    match (http::read_response(&mut &data[..], most_body), &whole.error) {
        (Ok(response), None) => {
            let head = (response.status, response.headers);
            assert_eq!(Some(&head), whole.head.as_ref(), "read_response reads the head the parser does");
            assert_eq!(response.body, whole.body, "read_response reads the body the parser does");
        }
        (Err(error), Some(failed)) => {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "read_response fails as InvalidData");
            let inner = error.get_ref().and_then(|it| it.downcast_ref::<HttpError>());
            assert_eq!(inner, Some(failed), "read_response fails with the parser's error");
        }
        (read, failed) => panic!("read_response gave {read:?} where the parser failed with {failed:?}"),
    }
}

/// Responses as the daemon and GitHub write them.
fn seeds() -> Vec<Vec<u8>> {
    let whole = http::response(200, &[("Content-Type", "application/cbor")], &[0xf6]);
    let mut chunked = http::chunked(200, &[("Content-Type", "application/x-ndjson")]);
    http::chunk_into(&mut chunked, b"{\"id\":1}\n");
    chunked.extend(http::chunk(&[0x55; 300]));
    chunked.extend(http::last_chunk());
    let mut interim = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
    interim.extend_from_slice(&whole);
    let until_end = b"HTTP/1.0 200 OK\r\nServer: x\r\n\r\nall of the rest".to_vec();
    let no_content = http::response(204, &[], &[]);
    let refused = http::response(401, &[("WWW-Authenticate", "Bearer")], b"a missing token");
    vec![Vec::new(), whole, chunked, interim, until_end, no_content, refused]
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
