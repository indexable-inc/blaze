//! A byte stream from a peer through blaze-protocol's frame readers: `read_frame`,
//! `read_frame_into` with one buffer kept from frame to frame, and `receive`, each from the
//! whole stream at once and from a reader that gives a few bytes a call and is interrupted
//! now and then; the frames read go back out through `write_frame`. One run takes one of the
//! four ways a frame reader is given the stream (`blaze_fuzzer::case`); the plain test takes
//! every way of each seed.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Every reader reads the frames a plain walk of the stream finds (a four-byte big-endian
//!   length of at most `LONGEST_FRAME`, then that many bytes), then ends as the walk does:
//!   `None` where the stream ends between frames, `UnexpectedEof` inside one, `InvalidData`
//!   for a length over the longest. How the bytes arrive changes nothing.
//! - A read asks for at most `blaze_fuzz::allowed(len)` bytes at once, whatever length a
//!   frame claims.
//! - `receive` gives what `cbor::from_slice` gives for each frame's item, its error as
//!   `InvalidData` carrying the decoder's.
//! - `write_frame` of the frames read writes back the stream up to where they end.

#![forbid(unsafe_code)]

use std::{
    io::{self, Read},
    time::Duration,
};

use blaze_fuzzer::Target;
use blaze_protocol::{LONGEST_FRAME, Message, cbor};
use serde::de::IgnoredAny;

const TARGET: Target = Target {
    name: "api_frame",
    check,
    seeds,
    dictionary: &[
        b"\x00\x00\x00\x00",
        b"\x00\x00\x00\x01",
        b"\x00\xff\xff\xff",
        b"\x01\x00\x00\x00",
        b"\x01\x00\x00\x01",
        b"\xff\xff\xff\xff",
        b"\xa3\x62id",
        b"\x64body",
        b"\x66schema\x02",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// How the stream ends, past its last whole frame.
#[derive(Debug, PartialEq)]
enum End {
    /// Between frames.
    Clean,
    /// Inside a frame's length or its item.
    Torn,
    /// At a length over the longest.
    TooLong,
}

fn end_of(error: &io::Error) -> End {
    match error.kind() {
        io::ErrorKind::UnexpectedEof => End::Torn,
        io::ErrorKind::InvalidData => End::TooLong,
        kind => panic!("a frame reader failed with {kind:?}, not a torn or a too long frame"),
    }
}

/// The frames of `data` as a plain walk finds them.
fn walk(data: &[u8]) -> (Vec<&[u8]>, End) {
    let mut frames = Vec::new();
    let mut at = 0;
    loop {
        let rest = &data[at..];
        if rest.is_empty() {
            return (frames, End::Clean);
        }
        let Some(&[a, b, c, d]) = rest.get(..4) else { return (frames, End::Torn) };
        let len = u32::from_be_bytes([a, b, c, d]);
        if len > LONGEST_FRAME {
            return (frames, End::TooLong);
        }
        let Some(item) = rest.get(4..4 + len as usize) else { return (frames, End::Torn) };
        frames.push(item);
        at += 4 + item.len();
    }
}

/// Gives 1 to 7 bytes a call, and is interrupted every fifth call.
struct Trickle<'a> {
    bytes: &'a [u8],
    calls: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls % 5 == 0 {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let n = out.len().min(self.bytes.len()).min(1 + self.calls % 7);
        out[..n].copy_from_slice(&self.bytes[..n]);
        self.bytes = &self.bytes[n..];
        Ok(n)
    }
}

/// `read_frame` from `from` reads `frames` and then ends as `end` says, each read asking for
/// at most `allowed` bytes at once.
fn read_frames(mut from: impl Read, frames: &[&[u8]], end: &End, allowed: usize) {
    let mut read = 0;
    let ended = loop {
        let (most, next) = blaze_fuzz::largest_allocation(|| blaze_protocol::read_frame(&mut from));
        assert!(most <= allowed, "a read of a frame asked for {most} bytes at once, past {allowed}");
        match next {
            Ok(Some(item)) => {
                assert_eq!(frames.get(read), Some(&&item[..]), "read_frame reads the frames there are");
                read += 1;
            }
            Ok(None) => break End::Clean,
            Err(error) => break end_of(&error),
        }
    };
    assert_eq!((read, &ended), (frames.len(), end), "read_frame ends where the stream does");
}

/// [`read_frames`] through `read_frame_into`, one buffer kept from frame to frame.
fn read_frames_into(mut from: impl Read, frames: &[&[u8]], end: &End, allowed: usize) {
    let mut item = vec![0xee; 3];
    let mut read = 0;
    let ended = loop {
        let (most, next) = blaze_fuzz::largest_allocation(|| {
            blaze_protocol::read_frame_into(&mut from, &mut item)
        });
        assert!(most <= allowed, "a read of a frame asked for {most} bytes at once, past {allowed}");
        match next {
            Ok(true) => {
                assert_eq!(frames.get(read), Some(&&item[..]), "read_frame_into reads the frames there are");
                read += 1;
            }
            Ok(false) => break End::Clean,
            Err(error) => break end_of(&error),
        }
    };
    assert_eq!((read, &ended), (frames.len(), end), "read_frame_into ends where the stream does");
}

/// `read_frame` and `read_frame_into`, each from the whole stream and through [`Trickle`].
const CASES: usize = 4;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let allowed = blaze_fuzz::allowed(data.len());
    let (frames, end) = walk(data);
    match case {
        0 => read_frames(data, &frames, &end, allowed),
        1 => read_frames(Trickle { bytes: data, calls: 0 }, &frames, &end, allowed),
        2 => read_frames_into(data, &frames, &end, allowed),
        _ => read_frames_into(Trickle { bytes: data, calls: 0 }, &frames, &end, allowed),
    }

    let mut from = data;
    for frame in &frames {
        let received = blaze_protocol::receive::<Message<IgnoredAny>>(&mut from);
        match (received, cbor::from_slice::<Message<IgnoredAny>>(frame)) {
            (Ok(Some(message)), Ok(decoded)) => assert_eq!(message, decoded, "receive reads what from_slice does"),
            (Err(error), Err(decoded)) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData, "receive refuses an item as InvalidData");
                let inner = error.get_ref().and_then(|it| it.downcast_ref::<cbor::Error>());
                assert_eq!(inner, Some(&decoded), "receive refuses an item with the decoder's error");
            }
            (received, decoded) => panic!("receive gave {received:?} where from_slice gave {decoded:?}"),
        }
    }
    match blaze_protocol::receive::<IgnoredAny>(&mut from) {
        Ok(None) => assert_eq!(end, End::Clean, "receive ends where the stream does"),
        Ok(Some(_)) => panic!("receive read a frame past the {} there are", frames.len()),
        Err(error) => assert_eq!(end_of(&error), end, "receive ends where the stream does"),
    }

    let mut written = Vec::new();
    for frame in &frames {
        blaze_protocol::write_frame(&mut written, frame).expect("a frame that was read can be written");
    }
    assert_eq!(written, data[..written.len()], "write_frame writes back the frames read");
}

/// Streams of messages as `send` writes them.
fn seeds() -> Vec<Vec<u8>> {
    let mut stream = Vec::new();
    blaze_protocol::send(&mut stream, &Message::new(1, "Status")).expect("a message has a frame");
    blaze_protocol::send(&mut stream, &Message::new(2, vec![1u64, 2, 3])).expect("a message has a frame");
    blaze_protocol::send(&mut stream, &Message::new(u64::MAX, ())).expect("a message has a frame");
    let mut big = Vec::new();
    blaze_protocol::send(&mut big, &Message::new(3, "x".repeat(300))).expect("a message has a frame");
    let mut wrong = Vec::new();
    blaze_protocol::write_frame(&mut wrong, &[0x18, 0x01]).expect("a frame of two bytes");
    let mut seeds = vec![Vec::new(), stream.clone(), big, wrong];
    stream.truncate(stream.len() - 1);
    seeds.push(stream);
    seeds
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
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }
}
