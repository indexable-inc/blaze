//! A connection's bytes through blaze-api's frame cutter (`frames::Frames`, what the daemon's
//! Unix side and the client cut items with), pushed all at once, a byte at a time, and in
//! pieces of uneven sizes, every whole item taken out after each push. One run takes one of
//! the cuts in pieces (`blaze_fuzzer::case`) beside the push all at once; the plain test takes
//! every cut of each seed.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - However the bytes are pushed, the items are the frames a plain walk of them finds (a
//!   four-byte big-endian length, then that many bytes), in order, and nothing else.
//! - The cutter waits (`Ok(None)`) while a frame is not whole, and at a length over
//!   `LONGEST_REQUEST` breaks with an error naming it before that frame's bytes arrive; once
//!   broken every `next` gives the same error and a push changes nothing.
//! - Pushing it all at once and taking the items out asks for at most
//!   `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_api::{ApiError, LONGEST_REQUEST, frames::Frames};
use blaze_fuzzer::Target;

const TARGET: Target = Target {
    name: "api_frames",
    check,
    seeds,
    dictionary: &[
        b"\x00\x00\x00\x00",
        b"\x00\x00\x00\x01",
        b"\x00\x00\x01\x00",
        b"\x01\x00\x00\x00",
        b"\x01\x00\x00\x01",
        b"\xff\xff\xff\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// What the cutter is doing once the items are out.
#[derive(Debug, PartialEq)]
enum Left {
    /// Waiting for more of a frame, or for the next.
    Waiting,
    Broken(ApiError),
}

/// The items of `data` as a plain walk finds them, and what is left.
fn walk(data: &[u8]) -> (Vec<&[u8]>, Left) {
    let mut items = Vec::new();
    let mut at = 0;
    while let Some(&[a, b, c, d]) = data.get(at..at + 4) {
        let len = u32::from_be_bytes([a, b, c, d]);
        if len > LONGEST_REQUEST {
            let what = format!("a frame of {len} bytes, longer than {LONGEST_REQUEST}");
            return (items, Left::Broken(ApiError::new(what, None)));
        }
        let Some(item) = data.get(at + 4..at + 4 + len as usize) else { break };
        items.push(item);
        at += 4 + item.len();
    }
    (items, Left::Waiting)
}

/// Every whole item the cutter has, then whether it waits or is broken.
fn drain(frames: &mut Frames, items: &mut Vec<Vec<u8>>) -> Option<ApiError> {
    loop {
        match frames.next() {
            Ok(Some(item)) => items.push(item),
            Ok(None) => return None,
            Err(error) => return Some(error),
        }
    }
}

/// `data` pushed in pieces of the sizes `sizes` gives, in turn, every item taken out after
/// each push.
fn cut(data: &[u8], sizes: &[usize]) -> (Vec<Vec<u8>>, Left) {
    let mut frames = Frames::new();
    let mut items = Vec::new();
    let mut at = 0;
    let mut piece = 0;
    while at < data.len() {
        let end = (at + sizes[piece % sizes.len()]).min(data.len());
        piece += 1;
        frames.push(&data[at..end]);
        at = end;
        if let Some(error) = drain(&mut frames, &mut items) {
            frames.push(&data[at..]);
            frames.push(&[0, 0, 0, 0]);
            assert_eq!(frames.next(), Err(error.clone()), "a broken cutter stays broken past a push");
            return (items, Left::Broken(error));
        }
    }
    (items, Left::Waiting)
}

/// The sizes of the pieces the bytes are pushed in, after all at once.
const CUTS: [&[usize]; 3] = [&[1], &[3, 1, 4, 1, 5, 9, 2, 6], &[4096, 7]];

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CUTS.len()));
}

fn check_case(data: &[u8], case: usize) {
    let (expected, left) = walk(data);

    let (most, (items, error)) = blaze_fuzz::largest_allocation(|| {
        let mut frames = Frames::new();
        frames.push(data);
        let mut items = Vec::new();
        let error = drain(&mut frames, &mut items);
        if let Some(error) = &error {
            assert_eq!(frames.next().as_ref(), Err(error), "a broken cutter gives its error again");
        }
        (items, error)
    });
    assert!(most <= blaze_fuzz::allowed(data.len()), "cutting {} bytes asked for {most} at once", data.len());
    assert_eq!(items, expected, "the items are the frames there are");
    assert_eq!(error.map_or(Left::Waiting, Left::Broken), left, "the cutter ends as the bytes do");

    let sizes = CUTS[case];
    let (items, ended) = cut(data, sizes);
    assert_eq!(items, expected, "the items are the frames there are, cut in pieces of {sizes:?}");
    assert_eq!(ended, left, "the cutter ends as the bytes do, cut in pieces of {sizes:?}");
}

/// Requests as the client frames them, one after another.
fn seeds() -> Vec<Vec<u8>> {
    use blaze_api::{Envelope, Request, frames};
    let mut stream = Vec::new();
    for (id, request) in [(1, Request::Status), (2, Request::Cancel { id: 1 }), (3, Request::Stop)] {
        stream.extend(frames::encode(&Envelope::new(id, request)).expect("a request has a frame"));
    }
    let mut torn = stream.clone();
    torn.truncate(torn.len() - 3);
    let mut long = stream.clone();
    long.extend((LONGEST_REQUEST + 1).to_be_bytes());
    long.extend([0; 16]);
    vec![Vec::new(), stream, torn, long]
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
        blaze_fuzzer::every_case(&super::TARGET, super::CUTS.len(), super::check_case);
    }
}
