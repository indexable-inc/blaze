use blaze_ids::rng::SplitMix64 as Rng;

use super::*;

fn parse(bytes: &[u8]) -> Result<Option<HttpRequest>, HttpError> {
    let mut parser = RequestParser::new(1 << 20);
    parser.push(bytes);
    parser.next()
}

fn status_of(bytes: &[u8]) -> u16 {
    parse(bytes).unwrap_err().status
}

#[test]
fn a_request_with_a_length_is_read_whole() {
    let bytes = request("POST", "daemon", "/api", &[("Content-Type", "application/cbor")], b"body");
    let request = parse(&bytes).unwrap().unwrap();
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/api"));
    assert_eq!(request.header("content-type"), Some("application/cbor"));
    assert_eq!(request.header("HOST"), Some("daemon"));
    assert_eq!(request.header("Content-Length"), Some("4"));
    assert_eq!(request.body, b"body");
}

#[test]
fn a_request_without_a_body_has_none() {
    let request = parse(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap().unwrap();
    assert_eq!(request.method, "GET");
    assert!(request.body.is_empty());
}

#[test]
fn a_chunked_body_is_decoded_with_extensions_and_trailers_dropped() {
    let bytes = b"POST /hook HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n\
        4;name=value\r\nWiki\r\n5\r\npedia\r\nE \r\n in\r\n\r\nchunks.\r\n0\r\nX-Trailer: 1\r\n\r\n";
    let request = parse(bytes).unwrap().unwrap();
    assert_eq!(request.body, b"Wikipedia in\r\n\r\nchunks.");
}

#[test]
fn a_request_comes_out_the_same_whatever_the_cuts() {
    let mut stream = request("POST", "h", "/a", &[("X-One", "1")], b"first body");
    stream.extend_from_slice(
        b"POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    );
    stream.extend(request("GET", "h", "/c", &[], b""));
    let whole = {
        let mut parser = RequestParser::new(1 << 20);
        parser.push(&stream);
        let mut out = Vec::new();
        while let Some(request) = parser.next().unwrap() {
            out.push(request);
        }
        out
    };
    assert_eq!(whole.len(), 3);
    assert_eq!(whole[1].body, b"abcde");
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut parser = RequestParser::new(1 << 20);
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(12) as usize).min(stream.len() - at);
            parser.push(&stream[at..at + len]);
            at += len;
            while let Some(request) = parser.next().unwrap() {
                out.push(request);
            }
        }
        assert_eq!(out, whole, "seed {seed}");
        assert_eq!(parser.held(), 0, "seed {seed}");
    }
}

/// Requests read in place are the owned ones, whatever the cuts, and say the same of what is
/// held; refusals are the same refusals.
#[test]
fn a_request_in_place_is_the_request() {
    let mut stream = request("POST", "h", "/a", &[("X-One", "1"), ("X-Two", " two\t")], b"first");
    stream.extend_from_slice(
        b"\r\nPOST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n",
    );
    stream.extend(request("GET", "h", "/c", &[], b""));
    stream.extend(request("PUT", "h", "/d", &[("Content-Type", "x")], &[7; 300]));
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let (mut owned, mut lent) = (RequestParser::new(1 << 20), RequestParser::new(1 << 20));
        let (mut want, mut got) = (Vec::new(), Vec::new());
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(40) as usize).min(stream.len() - at);
            owned.push(&stream[at..at + len]);
            match rng.below(2) == 0 {
                true => lent.push(&stream[at..at + len]),
                false => lent.push_vec(stream[at..at + len].to_vec()),
            }
            at += len;
            while let Some(request) = owned.next().unwrap() {
                want.push(request);
            }
            while let Some(request) = lent.next_ref().unwrap() {
                got.push(request.to_request());
                assert_eq!(request.header("x-two"), got.last().unwrap().header("X-Two"));
            }
            assert_eq!(lent.held(), owned.held(), "seed {seed}");
        }
        assert_eq!(got, want, "seed {seed}");
        assert_eq!(want.len(), 4);
    }
    let bad: [&[u8]; 6] = [
        b"GET / HTTP/1.1\nHost: x\r\n\r\n",
        b"GET / HTTP/1.1\r\nA: \xff\r\nB\r\n\r\n",
        b"GET / HTTP/1.1\r\nA: a\rb\r\nB\r\n\r\n",
        b"GET /  HTTP/1.1\r\nA\r\n\r\n",
        b"POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabc\r\n",
    ];
    for bytes in bad {
        let mut lent = RequestParser::new(1 << 20);
        lent.push(bytes);
        assert_eq!(lent.next_ref().unwrap_err(), parse(bytes).unwrap_err(), "{bytes:?}");
    }
    assert_eq!(parse(bad[1]).unwrap_err().what, "a head that is not UTF-8");
    assert_eq!(parse(bad[2]).unwrap_err().what, "a CR inside a head line");
}

/// A stream that gives its bytes in short reads of random size, some interrupted.
struct Pieces<'a> {
    bytes: &'a [u8],
    rng: Rng,
}

impl std::io::Read for Pieces<'_> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        if self.rng.below(8) == 0 {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let len = (1 + self.rng.below(40) as usize).min(into.len()).min(self.bytes.len());
        into[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        Ok(len)
    }
}

/// Reading into the parser is pushing what was read, connection after connection on one
/// parser reset between them, and a connection's room is kept for the next.
#[test]
fn a_parser_read_into_is_the_parser_pushed_to() {
    let mut stream = request("POST", "h", "/a", &[("X-One", "1")], &[3; 20_000]);
    stream.extend_from_slice(
        b"POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    );
    stream.extend(request("GET", "h", "/c", &[], b""));
    let mut read = RequestParser::new(1 << 20);
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let mut pushed = RequestParser::new(1 << 20);
        let (mut want, mut got) = (Vec::new(), Vec::new());
        read.reset();
        let mut from = Pieces { bytes: &stream, rng: Rng::new(seed + 1000) };
        loop {
            let at = stream.len() - from.bytes.len();
            let len = read.read_from(&mut from).unwrap();
            if len == 0 {
                break;
            }
            pushed.push(&stream[at..at + len]);
            while rng.below(3) != 0 {
                let one = pushed.next().unwrap();
                let two = read.next_ref().unwrap().map(|request| request.to_request());
                assert_eq!(two, one, "seed {seed}");
                let Some(one) = one else { break };
                want.push(one);
                got.push(two.unwrap());
            }
            assert_eq!(read.held(), pushed.held(), "seed {seed}");
        }
        while let Some(request) = pushed.next().unwrap() {
            want.push(request);
        }
        while let Some(request) = read.next_ref().unwrap() {
            got.push(request.to_request());
        }
        assert_eq!(got, want, "seed {seed}");
        assert_eq!(want.len(), 3, "seed {seed}");
        assert_eq!(read.held(), 0, "seed {seed}");
    }
    let broken = b"GET / HTTP/1.1\nHost: x\r\n\r\n";
    read.reset();
    let mut from = Pieces { bytes: broken, rng: Rng::new(7) };
    while read.next_ref().is_ok() {
        if let Ok(0) = read.read_from(&mut from) {
            panic!("the stream ended before the bad head was refused");
        }
    }
    assert_eq!(read.read_from(&mut from).unwrap(), 0, "a broken parser reads nothing");
    read.reset();
    read.push(&request("GET", "h", "/d", &[], b""));
    assert_eq!(read.next_ref().unwrap().unwrap().path, "/d", "reset clears what broke it");
}

/// The API server's read of a request: nothing allocated once the bytes are here.
#[test]
fn a_request_in_place_allocates_nothing() {
    let bytes = request("POST", "h", "/api", &[("Content-Type", "application/cbor")], &[1; 64]);
    let mut parser = RequestParser::new(1 << 20);
    for _ in 0..10 {
        parser.push(&bytes);
        let (path, body) = blaze_alloc_count::assert_at_most(0, "a request read in place", || {
            let request = parser.next_ref().unwrap().unwrap();
            (request.path.len(), request.body.len())
        });
        assert_eq!((path, body), (4, 64));
    }
}

/// A parser kept from connection to connection: a warm read into its room and a chunked
/// request decoded into its kept buffer allocate nothing.
#[test]
fn a_warm_read_and_chunked_request_allocate_nothing() {
    let chunked = b"POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
    let mut parser = RequestParser::new(1 << 20);
    let mut round = |parser: &mut RequestParser| {
        parser.reset();
        let mut from = &chunked[..];
        while parser.read_from(&mut from).unwrap() > 0 {}
        let request = parser.next_ref().unwrap().expect("a whole request");
        (request.path.len(), request.body == b"abcde")
    };
    round(&mut parser);
    for _ in 0..10 {
        let read = blaze_alloc_count::assert_at_most(0, "a warm read and a chunked request", || {
            round(&mut parser)
        });
        assert_eq!(read, (2, true));
    }
}

/// A whole response, request or chunked head is one allocation of its size, the bytes its
/// `_into` form writes onto a kept buffer, which allocates nothing warm.
#[test]
fn a_message_is_one_allocation_of_its_size() {
    let headers = [("Content-Type", "application/cbor"), ("X-Request-Id", "18446744073709551615")];
    let body = [5; 300];
    for status in [200, 299, 413, 431, 505] {
        let bytes = blaze_alloc_count::assert_at_most(1, "a response", || response(status, &headers, &body));
        let mut kept = Vec::with_capacity(1024);
        response_into(&mut kept, status, &headers, &body);
        assert_eq!(kept, bytes);
        kept.clear();
        blaze_alloc_count::assert_at_most(0, "a response onto a kept buffer", || {
            response_into(&mut kept, status, &headers, &body)
        });
        assert_eq!(kept, bytes);
        blaze_alloc_count::assert_at_most(1, "a chunked head", || chunked(status, &headers));
    }
    let long = "/".repeat(2000);
    let sent = blaze_alloc_count::assert_at_most(1, "a request", || request("PATCH", "api.github.com", &long, &headers, &body));
    let parsed = parse(&sent).unwrap().unwrap();
    assert_eq!((parsed.method.as_str(), parsed.path.len(), parsed.body.len()), ("PATCH", 2000, 300));
}

/// A response longer than one read is read whole, its head moved out of the parser.
#[test]
fn a_response_of_many_reads_is_read_whole() {
    let body: Vec<u8> = (0..100_000u32).map(|it| it as u8).collect();
    let bytes = response(200, &[("X-One", "1")], &body);
    let mut from = Pieces { bytes: &bytes, rng: Rng::new(3) };
    let read = read_response(&mut from, 1 << 20).unwrap();
    assert_eq!((read.status, read.header("x-one"), read.body == body), (200, Some("1"), true));
    let mut chunks = chunked(200, &[]);
    for piece in body.chunks(7_000) {
        chunk_into(&mut chunks, piece);
    }
    chunks.extend_from_slice(LAST_CHUNK);
    assert_eq!(read_response(&mut &chunks[..], 1 << 20).unwrap().body, body);
}

#[test]
fn blank_lines_before_a_request_are_skipped() {
    let request = parse(b"\r\n\r\nGET / HTTP/1.1\r\n\r\n").unwrap().unwrap();
    assert_eq!(request.path, "/");
}

#[test]
fn a_head_line_over_8_kib_is_refused_before_it_ends() {
    let mut bytes = b"GET / HTTP/1.1\r\nX-Long: ".to_vec();
    bytes.extend(std::iter::repeat_n(b'a', 8 << 10));
    let error = parse(&bytes).unwrap_err();
    assert_eq!(error.status, 431);
    assert!(error.what.contains("8 KiB"), "{error}");
}

#[test]
fn a_head_line_of_8_kib_is_read() {
    let mut bytes = b"GET / HTTP/1.1\r\n".to_vec();
    let mut line = b"X-Long: ".to_vec();
    line.resize(8 << 10, b'a');
    bytes.extend(line);
    bytes.extend(b"\r\n\r\n");
    assert!(parse(&bytes).unwrap().is_some());
}

#[test]
fn more_than_100_headers_are_refused() {
    let mut head = String::from("GET / HTTP/1.1\r\n");
    for n in 0..100 {
        head.push_str(&format!("X-{n}: v\r\n"));
    }
    let mut ok = head.clone();
    ok.push_str("\r\n");
    assert!(parse(ok.as_bytes()).unwrap().is_some());
    head.push_str("X-100: v\r\n\r\n");
    let error = parse(head.as_bytes()).unwrap_err();
    assert_eq!((error.status, error.what.as_str()), (431, "more than 100 headers"));
}

#[test]
fn a_body_over_the_bound_is_413_before_its_bytes_arrive() {
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nContent-Length: 11\r\n\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n0123456789");
    assert_eq!(parser.next().unwrap().unwrap().body, b"0123456789");
}

#[test]
fn a_chunked_body_over_the_bound_is_413() {
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nabcdef\r\n5\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
}

#[test]
fn both_a_length_and_chunks_are_refused() {
    let bytes = b"POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n";
    let error = parse(bytes).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("both"), "{error}");
}

#[test]
fn bad_lengths_and_codings_are_refused_naming_them() {
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nContent-Length: +3\r\n\r\nabc"), 400);
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\n"), 400);
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"), 501);
}

#[test]
fn bad_chunk_sizes_are_refused() {
    for size in ["", "x", "-1", "11111111111111111", " 5"] {
        let bytes = format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\n");
        let error = parse(bytes.as_bytes()).unwrap_err();
        assert_eq!((error.status, error.what.as_str()), (400, "a bad chunk size"), "{size:?}");
    }
    let bytes = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabc\r\n";
    assert_eq!(parse(bytes).unwrap_err().what, "a chunk not ended by CRLF");
}

#[test]
fn malformed_heads_are_refused() {
    assert_eq!(status_of(b"GET / HTTP/1.1\nHost: x\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\n folded\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\nNo colon\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\nBad name: x\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET /  HTTP/1.1\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/2.0\r\n\r\n"), 505);
    assert_eq!(status_of(b"GET / FTP/1.1\r\n\r\n"), 400);
}

#[test]
fn a_parser_that_refused_keeps_refusing() {
    let mut parser = RequestParser::new(100);
    parser.push(b"GET / HTTP/2.0\r\n\r\n");
    let error = parser.next().unwrap_err();
    parser.push(b"GET / HTTP/1.1\r\n\r\n");
    assert_eq!(parser.next().unwrap_err(), error);
}

#[test]
fn a_response_round_trips_through_the_parser() {
    let bytes = response(404, &[("Content-Type", "text/plain")], b"no such path");
    assert!(bytes.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
    let response = read_response(&mut &bytes[..], 1 << 20).unwrap();
    assert_eq!(response.status, 404);
    assert_eq!(response.header("content-type"), Some("text/plain"));
    assert_eq!(response.body, b"no such path");
}

#[test]
fn a_chunked_response_hands_its_body_over_as_it_comes() {
    let mut bytes = chunked(200, &[("Content-Type", "application/cbor")]);
    bytes.extend(chunk(b"one"));
    bytes.extend(chunk(b""));
    bytes.extend(chunk(b"two"));
    bytes.extend(last_chunk());
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let mut parser = ResponseParser::new(1 << 20);
        let mut body = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            // No empty pushes: one is the end of the stream.
            let len = (1 + rng.below(7) as usize).min(bytes.len() - at);
            parser.push(&bytes[at..at + len]).unwrap();
            at += len;
            body.extend(parser.take_body());
        }
        assert!(parser.finished(), "seed {seed}");
        assert_eq!(parser.head().unwrap().0, 200);
        assert_eq!(body, b"onetwo", "seed {seed}");
    }
}

#[test]
fn an_empty_chunk_is_nothing_and_never_the_last() {
    assert!(chunk(b"").is_empty());
    assert_eq!(chunk(b"abc"), b"3\r\nabc\r\n");
    assert_eq!(chunk(&[0; 26])[..4], *b"1a\r\n");
    assert_eq!(last_chunk(), b"0\r\n\r\n");
}

#[test]
fn bytes_after_a_response_are_the_next_ones() {
    let mut bytes = response(200, &[], b"a");
    bytes.extend(response(201, &[], b"b"));
    let mut parser = ResponseParser::new(100);
    parser.push(&bytes).unwrap();
    assert!(parser.finished());
    assert_eq!(parser.take_body(), b"a");
    let rest = parser.rest();
    let mut next = ResponseParser::new(100);
    next.push(&rest).unwrap();
    assert_eq!((next.head().unwrap().0, next.take_body()), (201, b"b".to_vec()));
}

#[test]
fn an_interim_response_is_skipped() {
    let mut bytes = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
    bytes.extend(response(200, &[], b"done"));
    let response = read_response(&mut &bytes[..], 100).unwrap();
    assert_eq!((response.status, response.body), (200, b"done".to_vec()));
}

#[test]
fn a_response_without_a_length_ends_with_the_stream() {
    let bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nall of it";
    let response = read_response(&mut &bytes[..], 100).unwrap();
    assert_eq!(response.body, b"all of it");
    let mut parser = ResponseParser::new(100);
    parser.push(bytes).unwrap();
    assert!(!parser.finished());
    parser.push(&[]).unwrap();
    assert!(parser.finished());
}

#[test]
fn a_response_cut_short_or_too_long_is_an_error() {
    let bytes = response(200, &[], b"twelve bytes");
    assert!(read_response(&mut &bytes[..bytes.len() - 1], 100).is_err());
    assert!(read_response(&mut &bytes[..10], 100).is_err());
    let error = read_response(&mut &bytes[..], 11).unwrap_err();
    assert!(error.to_string().contains("413"), "{error}");
}

#[test]
fn no_content_has_no_body() {
    let response = read_response(&mut &b"HTTP/1.1 204 No Content\r\n\r\n"[..], 100).unwrap();
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
}

#[test]
fn a_status_line_without_a_reason_is_read() {
    let response = read_response(&mut &b"HTTP/1.1 299\r\nContent-Length: 0\r\n\r\n"[..], 1).unwrap();
    assert_eq!(response.status, 299);
}

/// The server's way in: each read fills the buffer the last `push_vec` left free. A request in
/// two reads takes both ways through `push_vec` (adopted, then copied), and warm, neither
/// allocates.
#[test]
fn a_warm_read_into_the_spare_allocates_nothing() {
    let bytes = request("POST", "h", "/api", &[("Content-Type", "application/cbor")], &[1; 64]);
    let (head, tail) = bytes.split_at(bytes.len() / 2);
    let mut parser = RequestParser::new(1 << 20);
    let round = |parser: &mut RequestParser| {
        for piece in [head, tail] {
            let mut into = parser.lend_spare();
            assert!(into.is_empty());
            into.reserve(READ);
            into.extend_from_slice(piece);
            parser.push_vec(into);
        }
        let request = parser.next_ref().unwrap().expect("a whole request");
        (request.path.len(), request.body.len())
    };
    for _ in 0..2 {
        round(&mut parser);
    }
    for _ in 0..10 {
        let read = blaze_alloc_count::assert_at_most(0, "a warm read into the spare", || {
            round(&mut parser)
        });
        assert_eq!(read, (4, 64));
    }
}

/// What a broken parser is given is still left free for the next read.
#[test]
fn a_broken_parser_still_lends_its_spare() {
    let mut parser = RequestParser::new(1 << 20);
    parser.push(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n");
    assert!(parser.next_ref().is_err());
    let mut into = Vec::with_capacity(READ);
    into.extend_from_slice(b"more");
    parser.push_vec(into);
    let spare = parser.lend_spare();
    assert!(spare.is_empty() && spare.capacity() >= READ);
}
