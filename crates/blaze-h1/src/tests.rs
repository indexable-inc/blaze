//! The crate's promises, each with its oracle and, for the stateful parser, a planted bug:
//!
//! - a response's body is the same however its bytes are split between feeds;
//! - framing that two readers could take apart differently is refused;
//! - a range is taken only when it is the range asked for;
//! - redirects stop at the policy's count and scheme;
//! - every cap of [`Limits`] and the redirect count, drawn down to its least value, refuses
//!   exactly the response past it (D165), each cap reached.

use core::fmt;
use std::{format, vec::Vec as StdVec};

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls};

use crate::{
    Error, Follow, H1Plant, Limits, Method, Origin, Range, Redirects, Request, Response, ResponseOpts,
    Why, write_request,
};

const LIMITS: Limits = Limits { most_head: 16 << 10, most_headers: 64, most_body: 1 << 24, most_chunk_line: 4096 };

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// A parser held to `limits` for an answer to `method`, in the test's heap.
fn parser(limits: Limits, method: Method) -> Response<Alloc<'static>> {
    Response::new(ResponseOpts { limits, method, alloc: heap() })
}

/// The method of a request, HEAD or not.
fn method(head: bool) -> Method {
    if head { Method::Head } else { Method::Other }
}

/// `response` fed in the pieces `cuts` makes (offsets, ascending), then the connection closed.
fn parse(response: &[u8], cuts: &[usize], head: bool) -> Result<(u16, StdVec<u8>), Error> {
    parse_planted(LIMITS, response, cuts, head, NoPlant)
}

/// `parse` held to `limits`, its feeds planted with `plant`.
fn parse_planted(
    limits: Limits,
    response: &[u8],
    cuts: &[usize],
    head: bool,
    plant: impl Planted<H1Plant>,
) -> Result<(u16, StdVec<u8>), Error> {
    let mut r = parser(limits, method(head));
    let mut out = Vec::new_in(heap());
    let mut from = 0;
    for &cut in cuts.iter().chain(core::iter::once(&response.len())) {
        let cut = cut.clamp(from, response.len());
        let fed = r.feed_planted(&response[from..cut], &mut out, plant)?;
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

/// The invariant of the split oracle: one body however the response is cut.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SplitBroken {
    /// Cut at `cut` (or, `None`, at many places), the body read is not the body sent: refused
    /// for `refused`, or other bytes.
    Differs { cut: Option<usize>, refused: Option<Why> },
}

impl fmt::Display for SplitBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let SplitBroken::Differs { cut, refused } = self;
        match cut {
            Some(cut) => write!(f, "the chunked body differs when cut at {cut}")?,
            None => f.write_str("the chunked body differs when cut in many places")?,
        }
        match refused {
            Some(why) => write!(f, ": refused: {}", Error { at: 0, why: *why }),
            None => Ok(()),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the parser, each with the invariant that must catch it.
    enum SplitPlant breaks SplitBroken {
        /// The parser as it is.
        None,
        /// A size line cut between feeds loses its first digits: the body read is not the sent.
        SizeLineRestarts => SplitBroken::Differs { .. },
    }
}

impl SplitPlant {
    /// The parser's plant this control plants.
    fn planted(self) -> H1Plant {
        match self {
            SplitPlant::None => H1Plant::None,
            SplitPlant::SizeLineRestarts => H1Plant::SizeLineRestarts,
        }
    }
}

fn every_split(tape: &Tape, plant: impl Planted<H1Plant>) -> Result<(), Broken<SplitBroken>> {
    let body = tape.bytes(0..=700, "body");
    let response = chunked(tape, &body);
    let differs = |cut, refused| Broken::Oracle(SplitBroken::Differs { cut, refused });
    for cut in 0..=response.len() {
        let (status, got) = parse_planted(LIMITS, &response, &[cut], false, plant)
            .map_err(|e| differs(Some(cut), Some(e.why)))?;
        if status != 200 || got != body {
            return Err(differs(Some(cut), None));
        }
    }
    let cuts = {
        let mut cuts = tape.vec(0..=20, "cuts", |t| t.index(response.len() + 1, "cut"));
        cuts.sort_unstable();
        cuts
    };
    let (_, got) = parse_planted(LIMITS, &response, &cuts, false, plant).map_err(|e| differs(None, Some(e.why)))?;
    if got != body {
        return Err(differs(None, None));
    }
    Ok(())
}

#[test]
fn a_chunked_body_is_the_same_at_every_split() {
    check(Config::cases(48), |tape| every_split(tape, NoPlant));
}

#[test]
fn every_parser_plant_is_caught_by_its_invariant() {
    controls(Config::cases(48), |tape, plant: SplitPlant| every_split(tape, plant.planted()));
}

// The caps, drawn (D165): each field of `Limits` and the redirect count down to its least
// value, the response built small enough that every cap is met and passed.

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    fn draw(tape: &Tape) -> Limits {
        let shipped = Limits::SHIPPED;
        Limits {
            most_head: tape.limit(shipped.most_head, 1, "most head"),
            most_headers: tape.limit(shipped.most_headers, 0, "most headers"),
            most_body: tape.limit(shipped.most_body, 0, "most body"),
            most_chunk_line: tape.limit(shipped.most_chunk_line, 1, "most chunk line"),
        }
    }
}

blaze_sim::points! {
    /// Where the caps' simulation has to get.
    enum CapPoint {
        /// A head past `most_head` was refused.
        HeadOver,
        /// More header lines than `most_headers` were refused.
        HeadersOver,
        /// A body past `most_body` was refused.
        BodyOver,
        /// A chunk-size line past `most_chunk_line` was refused.
        ChunkLineOver,
        /// A response within every cap was read whole.
        Read,
        /// A redirect past the policy's `most` was refused.
        RedirectsOver,
        /// A redirect within the policy's `most` was followed.
        Followed,
    }
}

/// The invariant of the caps' oracle: the parser refuses a response for a cap exactly when the
/// response passes it, naming the cap, and reads the body of one within every cap.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CapBroken {
    /// The parser's answer is not the one the caps say.
    Disagrees { expected: Result<StdVec<u8>, Why>, got: Result<StdVec<u8>, Why> },
    /// The redirect at `hop` was answered other than the policy's count says.
    Redirect { hop: u8, most: u8, got: Result<bool, Why> },
}

impl fmt::Display for CapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapBroken::Disagrees { expected, got } => {
                write!(f, "the caps say {expected:?}, the parser answered {got:?}")
            }
            CapBroken::Redirect { hop, most, got } => {
                write!(f, "redirect {hop} under a policy of at most {most} was answered {got:?}")
            }
        }
    }
}

/// One case: the caps drawn, then a chunked response of a few headers and one chunk with an
/// extension, each part sized so that the drawn caps are met and passed, read whole and judged
/// against what the caps say; then a redirect loop under a drawn count.
fn caps(tape: &Tape) -> Result<(), Broken<CapBroken>> {
    let limits: Limits = tape.limits();
    let headers = tape.int_in(0usize..=3, "headers");
    let ext = tape.int_in(0usize..=6, "extension");
    let data = tape.bytes(1..=8, "data");
    let mut head = StdVec::from(&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"[..]);
    for _ in 0..headers {
        head.extend_from_slice(b"X-N: v\r\n");
    }
    head.extend_from_slice(b"\r\n");
    let mut size_line = format!("{:x}", data.len()).into_bytes();
    if ext > 0 {
        size_line.push(b';');
        size_line.extend(core::iter::repeat_n(b'a', ext));
    }
    size_line.extend_from_slice(b"\r\n");
    let mut bytes = head.clone();
    bytes.extend_from_slice(&size_line);
    bytes.extend_from_slice(&data);
    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
    let line_most = usize::from(limits.most_chunk_line);
    let expected = if head.len() > limits.most_head as usize {
        Err(Why::HeadOver { most: limits.most_head })
    } else if headers + 1 > usize::from(limits.most_headers) {
        Err(Why::HeadersOver { most: limits.most_headers })
    } else if size_line.len() > line_most {
        Err(Why::ChunkLineOver { most: limits.most_chunk_line })
    } else if data.len() as u64 > limits.most_body {
        Err(Why::BodyOver { most: limits.most_body })
    } else if b"0\r\n".len() > line_most {
        Err(Why::ChunkLineOver { most: limits.most_chunk_line })
    } else {
        Ok(data.to_vec())
    };
    let got = parse_planted(limits, &bytes, &[], false, NoPlant).map(|(_, body)| body).map_err(|e| e.why);
    if got != expected {
        return Err(Broken::Oracle(CapBroken::Disagrees { expected, got }));
    }
    match got {
        Ok(_) => tape.reach(CapPoint::Read),
        Err(Why::HeadOver { .. }) => tape.reach(CapPoint::HeadOver),
        Err(Why::HeadersOver { .. }) => tape.reach(CapPoint::HeadersOver),
        Err(Why::BodyOver { .. }) => tape.reach(CapPoint::BodyOver),
        Err(Why::ChunkLineOver { .. }) => tape.reach(CapPoint::ChunkLineOver),
        Err(_) => {}
    }
    redirects(tape)
}

/// The same `Location` again and again under a drawn count: followed while under it, refused
/// at it, by the count.
fn redirects(tape: &Tape) -> Result<(), Broken<CapBroken>> {
    let most = tape.limit(5u8, 0, "most redirects");
    let hops = tape.int_in(0u8..=7, "hops");
    let policy = Redirects { most, cross_scheme: false, cross_origin: true };
    let from = Origin { scheme: "https", authority: "crates.io" };
    let mut follow = Follow::new(policy);
    for hop in 0..hops {
        let got = follow.next(from, 302, at("/again")).map(|next| next.is_some());
        let expected = if hop < most { Ok(true) } else { Err(Why::RedirectsOver { most }) };
        if got != expected {
            return Err(Broken::Oracle(CapBroken::Redirect { hop, most, got }));
        }
        match got {
            Ok(_) => tape.reach(CapPoint::Followed),
            Err(_) => {
                tape.reach(CapPoint::RedirectsOver);
                break;
            }
        }
    }
    Ok(())
}

/// Each cap is passed only when the caps before it in the response are met, and a drawn cap is
/// mostly at its least value, so the later caps (the body, a whole read) are rare: the cases are
/// many, and each is a parse of under 100 bytes.
#[test]
fn every_cap_refuses_exactly_what_passes_it() {
    check(Config::cases(4000).must_reach_points(<CapPoint as Point>::IDS), caps);
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
    let mut r = parser(small, Method::Other);
    let got = r.feed(b"HTTP/1.1 200 OK\r\nX-Long: aaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n", &mut Vec::new_in(heap()));
    assert_eq!(got.map_err(|e| e.why), Err(Why::HeadOver { most: 32 }));
    let small = Limits { most_body: 4, ..LIMITS };
    // The deprecated constructor still answers as `new` does.
    let mut r = Response::new_in(small, false, heap());
    let got = r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", &mut Vec::new_in(heap()));
    assert_eq!(got.map_err(|e| e.why), Err(Why::BodyOver { most: 4 }));
    let small = Limits { most_chunk_line: 8, ..LIMITS };
    let mut r = parser(small, Method::Other);
    let got = r.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;abcdefgh\r\n", &mut Vec::new_in(heap()));
    assert_eq!(got.map_err(|e| e.why), Err(Why::ChunkLineOver { most: 8 }));
}

fn ranged(response: &[u8], asked: Range) -> Result<(u64, u64), Why> {
    let mut r = parser(LIMITS, Method::Other);
    r.feed(response, &mut Vec::new_in(heap())).map_err(|e| e.why)?;
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
    let mut out = Vec::new_in(heap());
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
    let refuse = |r: Request<'_>| write_request(&r, &mut Vec::new_in(heap())).map_err(|e| e.why);
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
        let mut r = parser(LIMITS, Method::Other);
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
        let mut r = parser(LIMITS, method(method_head));
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
