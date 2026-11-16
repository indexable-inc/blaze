//! The crate's promises, each with its oracle and, for the stateful parser, a planted bug:
//!
//! - a response's body is the same however its bytes are split between feeds;
//! - framing that two readers could take apart differently is refused;
//! - a range is taken only when it is the range asked for;
//! - redirects stop at the policy's count and scheme.

use std::{alloc::Global, format, string::String, vec::Vec as StdVec};

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::{
    Error, Follow, Limits, Origin, Range, Redirects, Request, Response, Why, write_request,
    plant::{self, Plant},
};

const LIMITS: Limits = Limits { most_head: 16 << 10, most_headers: 64, most_body: 1 << 24 };

/// `response` fed in the pieces `cuts` makes (offsets, ascending), then the connection closed.
fn parse(response: &[u8], cuts: &[usize], head: bool) -> Result<(u16, StdVec<u8>), Error> {
    let mut r = Response::new_in(LIMITS, head, Global);
    let mut out = Vec::new_in(Global);
    let mut from = 0;
    for &cut in cuts.iter().chain(core::iter::once(&response.len())) {
        let cut = cut.clamp(from, response.len());
        let fed = r.feed(&response[from..cut], &mut out)?;
        if fed.done && fed.consumed < cut - from {
            return Err(Error { at: 0, why: Why::AfterEnd });
        }
        from = cut;
    }
    r.closed()?;
    Ok((r.status(), out.as_slice().to_vec()))
}

/// A chunked response of `body`, cut into chunks of the drawn sizes, with extensions and a
/// trailer now and then.
fn chunked(tape: &Tape, body: &[u8]) -> StdVec<u8> {
    let mut out = StdVec::from(&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: b\r\n\r\n"[..]);
    let mut at = 0;
    while at < body.len() {
        let size = tape.int_in(1usize..=300, "chunk").min(body.len() - at);
        out.extend_from_slice(format!("{size:x}").as_bytes());
        if tape.coin("extension") {
            out.extend_from_slice(b";name=value");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&body[at..at + size]);
        out.extend_from_slice(b"\r\n");
        at += size;
    }
    out.extend_from_slice(b"0\r\n");
    if tape.coin("trailer") {
        out.extend_from_slice(b"Checksum: abc\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn every_split(tape: &Tape) -> Result<(), Broke> {
    let body = tape.bytes(0..=700, "body");
    let response = chunked(tape, &body);
    for cut in 0..=response.len() {
        let (status, got) = parse(&response, &[cut], false)
            .map_err(|e| Broke::Oracle(format!("chunked body differs: refused when cut at {cut}: {e}")))?;
        if status != 200 || got != body {
            return Err(Broke::Oracle(format!("chunked body differs when cut at {cut}")));
        }
    }
    let cuts = {
        let mut cuts = tape.vec(0..=20, "cuts", |t| t.index(response.len() + 1, "cut"));
        cuts.sort_unstable();
        cuts
    };
    let (_, got) =
        parse(&response, &cuts, false).map_err(|e| Broke::Oracle(format!("chunked body differs: refused: {e}")))?;
    if got != body {
        return Err(Broke::Oracle(String::from("chunked body differs when cut in many places")));
    }
    Ok(())
}

#[test]
fn a_chunked_body_is_the_same_at_every_split() {
    check(Config::cases(48), every_split);
}

#[test]
fn the_oracle_catches_a_size_line_lost_at_a_split() {
    control(Config::cases(48), Plant::Clean, Plant::SizeLineRestarts, "differs", |tape, p| {
        plant::with(p, || every_split(tape))
    });
}

#[test]
fn length_close_and_empty_bodies() {
    let r = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    assert_eq!(parse(r, &[3, 30], false).expect("length"), (200, b"hello".to_vec()));
    let r = b"HTTP/1.0 200 OK\r\n\r\nuntil the end";
    assert_eq!(parse(r, &[], false).expect("close").1, b"until the end");
    let r = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
    assert_eq!(parse(r, &[], true).expect("head").1, b"");
    let r = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n";
    assert_eq!(parse(r, &[], false).expect("interim").0, 204);
    let r = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhello";
    assert_eq!(parse(r, &[], false).map_err(|e| e.why), Err(Why::Truncated { body: 5 }));
}

#[test]
fn ambiguous_framing_is_refused() {
    let refused = |r: &[u8]| parse(r, &[], false).map_err(|e| e.why);
    assert_eq!(
        refused(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n"),
        Err(Why::LengthAndChunked)
    );
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\n"), Err(Why::ContentLength));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nContent-Length: +3\r\n\r\n"), Err(Why::ContentLength));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"), Err(Why::TransferEncoding));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nX-Bad : v\r\n\r\n"), Err(Why::HeaderLine));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nX: a\r\n folded\r\n\r\n"), Err(Why::HeaderLine));
    assert_eq!(refused(b"HTTP/2 200\r\n\r\n"), Err(Why::StatusLine));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"), Err(Why::ChunkSize));
    assert_eq!(refused(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nab\r\n"), Err(Why::ChunkEnd));
    let small = Limits { most_head: 32, ..LIMITS };
    let mut r = Response::new_in(small, false, Global);
    let got = r.feed(b"HTTP/1.1 200 OK\r\nX-Long: aaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n", &mut Vec::new_in(Global));
    assert_eq!(got.map_err(|e| e.why), Err(Why::HeadOver { most: 32 }));
    let small = Limits { most_body: 4, ..LIMITS };
    let mut r = Response::new_in(small, false, Global);
    let got = r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", &mut Vec::new_in(Global));
    assert_eq!(got.map_err(|e| e.why), Err(Why::BodyOver { most: 4 }));
}

fn ranged(response: &[u8], asked: Range) -> Result<(u64, u64), Why> {
    let mut r = Response::new_in(LIMITS, false, Global);
    r.feed(response, &mut Vec::new_in(Global)).map_err(|e| e.why)?;
    r.ranged(asked).map(|got| (got.first, got.last)).map_err(|e| e.why)
}

#[test]
fn a_range_is_taken_only_as_asked() {
    let asked = Range { first: 100, last: Some(199) };
    let ok = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-199/1000\r\nContent-Length: 100\r\n\r\n";
    assert_eq!(ranged(ok, asked), Ok((100, 199)));
    // Short because the resource ends there.
    let short = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-149/150\r\nContent-Length: 50\r\n\r\n";
    assert_eq!(ranged(short, asked), Ok((100, 149)));
    // Short for no reason, or elsewhere.
    let cut = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-149/1000\r\nContent-Length: 50\r\n\r\n";
    assert_eq!(ranged(cut, asked), Err(Why::RangeMismatch { first: 100, last: 149 }));
    let other = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-99/1000\r\nContent-Length: 100\r\n\r\n";
    assert_eq!(ranged(other, asked), Err(Why::RangeMismatch { first: 0, last: 99 }));
    // A length that is not the range's.
    let long = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-199/1000\r\nContent-Length: 101\r\n\r\n";
    assert_eq!(ranged(long, asked), Err(Why::RangeMismatch { first: 100, last: 199 }));
    // The whole resource for a ranged request: refused, not taken as the range.
    let whole = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n";
    assert_eq!(ranged(whole, asked), Err(Why::RangeStatus { status: 200 }));
    let open = Range { first: 900, last: None };
    let tail = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 900-999/1000\r\nContent-Length: 100\r\n\r\n";
    assert_eq!(ranged(tail, open), Ok((900, 999)));
}

#[test]
fn redirects_are_bounded_and_keep_their_scheme() {
    let policy = Redirects { most: 5, cross_scheme: false, cross_origin: true };
    let from = Origin { scheme: "https", authority: "crates.io" };
    // A loop: the same Location again and again stops at the count.
    let mut follow = Follow::new(policy);
    let mut stopped = None;
    for i in 0..100 {
        match follow.next(from, 302, at("/api/v1/crates/x/download")) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("302 is a redirect"),
            Err(why) => {
                stopped = Some((i, why));
                break;
            }
        }
    }
    assert_eq!(stopped, Some((5, Why::RedirectsOver { most: 5 })));
    let mut follow = Follow::new(policy);
    let hop = follow.next(from, 302, at("https://static.crates.io/crates/x/x-1.0.0.crate#f")).expect("cross-origin");
    let hop = hop.expect("a hop");
    assert_eq!((hop.origin.authority, hop.target), ("static.crates.io", "/crates/x/x-1.0.0.crate"));
    assert_eq!(follow.next(from, 301, at("http://crates.io/")), Err(Why::RedirectScheme));
    assert_eq!(follow.next(from, 307, at("../relative")), Err(Why::Location));
    assert_eq!(follow.next(from, 307, at("https://user@evil/")), Err(Why::Location));
    assert_eq!(follow.next(from, 308, None), Err(Why::NoLocation));
    assert_eq!(follow.next(from, 200, None), Ok(None));
    let same = Redirects { cross_origin: false, ..policy };
    let mut follow = Follow::new(same);
    assert_eq!(follow.next(from, 302, at("https://elsewhere/x")), Err(Why::RedirectOrigin));
    assert!(follow.next(from, 302, at("https://crates.io:443/x")).is_ok());
}

fn at(location: &str) -> Option<&[u8]> {
    Some(location.as_bytes())
}

#[test]
fn requests_are_written_in_one_order_and_checked() {
    let mut out = Vec::new_in(Global);
    let request = Request {
        method: "GET",
        target: "/pack/abc",
        host: "cache.example:8443",
        range: Some(Range { first: 10, last: Some(19) }),
        headers: &[("User-Agent", "blaze"), ("Accept", "*/*")],
        body_len: None,
        close: true,
    };
    write_request(&request, &mut out).expect("written");
    assert_eq!(
        &out[..],
        &b"GET /pack/abc HTTP/1.1\r\nHost: cache.example:8443\r\nRange: bytes=10-19\r\nUser-Agent: blaze\r\nAccept: */*\r\nConnection: close\r\n\r\n"[..]
    );
    let refuse = |r: Request<'_>| write_request(&r, &mut Vec::new_in(Global)).map_err(|e| e.why);
    assert_eq!(refuse(Request { headers: &[("X", "a\r\nInjected: 1")], ..request }), Err(Why::HeaderValue { index: 0 }));
    assert_eq!(refuse(Request { headers: &[("Bad Name", "a")], ..request }), Err(Why::HeaderName { index: 0 }));
    assert_eq!(refuse(Request { headers: &[("a", "b"), ("host", "evil")], ..request }), Err(Why::HeaderName { index: 1 }));
    assert_eq!(refuse(Request { target: "/a b", ..request }), Err(Why::Target));
    assert_eq!(refuse(Request { range: Some(Range { first: 5, last: Some(4) }), ..request }), Err(Why::RangeOrder { first: 5, last: 4 }));
}

/// `feed_head` stops where the body starts, interim heads read past; the rest is the body's.
/// A connection is kept only as RFC 9112 section 9.3 says.
#[test]
fn the_head_alone_is_taken_and_the_connection_kept_by_the_rules() {
    let bytes = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc";
    for cut in 1..bytes.len() {
        let mut r = Response::new_in(LIMITS, false, Global);
        let first = r.feed_head(&bytes[..cut]).expect("a head");
        let second = r.feed_head(&bytes[first.consumed..]).expect("a head");
        assert_eq!(first.consumed + second.consumed, bytes.len() - 3, "cut at {cut}");
        assert!(r.has_head() && !second.done && r.content_length() == Some(3), "cut at {cut}");
        let mut out = StdVec::new();
        assert_eq!(r.feed(b"abc", &mut out).map(|it| it.done), Ok(true));
        assert_eq!(out, b"abc");
        assert!(r.reusable());
    }
    let kept = |head: &[u8], method_head: bool| {
        let mut r = Response::new_in(LIMITS, method_head, Global);
        r.feed_head(head).expect("a head");
        (r.reusable(), r.content_length())
    };
    assert_eq!(kept(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", false), (true, Some(0)));
    assert_eq!(kept(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n", false), (false, Some(0)));
    assert_eq!(kept(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", false), (false, Some(0)));
    assert_eq!(kept(b"HTTP/1.0 200 OK\r\nConnection: Keep-Alive\r\nContent-Length: 0\r\n\r\n", false), (true, Some(0)));
    assert_eq!(kept(b"HTTP/1.1 200 OK\r\n\r\n", false), (false, None));
    assert_eq!(kept(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", true), (true, None));
}
