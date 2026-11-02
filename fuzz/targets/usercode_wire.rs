//! What a test binary (user code) writes on the fork-server socket, through blaze-usercode's
//! `test::wire`: `Frames` (`push`, `push_owned`, `pop`, `waiting`, `needed`), as the queue
//! reads a server's bytes, and `Frame::decode`. The input's first byte says how the rest, the
//! stream, is cut: its low three bits a count of piece sizes that follow (each a byte, one
//! less than the size), bit 3 whether every other piece goes in by `push_owned`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Pushed whole, a byte at a time, in pieces of exactly what `needed` asks for, in the
//!   input's pieces and in fixed uneven ones, `pop` gives the frames a plain walk of the stream
//!   finds (a little-endian `u32` length, then a body `Frame::decode` reads), in order, up to
//!   and with the first error, the model of `frames_read_in_place_as_a_whole_stream_reads`;
//!   with no error, `waiting` is then the bytes of a frame not yet whole.
//! - A length past `LONGEST` is an error once its four bytes are in, before any of its body.
//! - `needed` is 4 less what of a length has come, then what makes the frame whole (its length
//!   capped at `LONGEST`), so never 0 and never past `4 + LONGEST`; once the length is in, a
//!   push of exactly that makes `pop` give a frame or an error.
//! - A body `Frame::decode` reads is exactly the bytes `Frame::encode` writes for that frame,
//!   for each body of the stream and for the stream itself read as one body.
//! - Pushing the stream whole and popping every frame asks for at most
//!   `blaze_fuzz::allowed(len)` bytes at once: no length is believed before its bytes come.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use blaze_usercode::test::wire::{End, Executed, Frame, Frames, LONGEST};

const TARGET: Target = Target {
    name: "usercode_wire",
    check,
    seeds,
    dictionary: &[
        b"\x01",
        b"\x02",
        b"\x81",
        b"\x82",
        b"\x83",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
        b"\x00\x00\x00\x04",
        b"\x01\x00\x00\x04",
        b"\xff\xff\xff\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// How a stream ends once its frames are out.
#[derive(Clone, Debug, PartialEq)]
enum Ended {
    /// With this many bytes of a frame not yet whole.
    Waiting(usize),
    Broken(String),
}

/// The bytes `Frame::encode` writes for `frame`, without the length.
fn body_of(frame: &Frame) -> Vec<u8> {
    let mut bytes = frame.to_bytes();
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(len, bytes.len() - 4, "a frame's length is its body's");
    bytes.drain(..4);
    bytes
}

/// `Frame::decode` of `body`, its answer checked against the writer's bytes.
fn decode(body: &[u8]) -> Result<Frame, String> {
    let frame = Frame::decode(body)?;
    assert_eq!(body_of(&frame), body, "a body decoded is the bytes its frame encodes to: {frame:?}");
    Ok(frame)
}

/// The frames of `stream` as a plain walk of it reads them, and how it ends.
fn walk(stream: &[u8]) -> (Vec<Frame>, Ended) {
    let mut frames = Vec::new();
    let mut at = 0;
    loop {
        let rest = &stream[at..];
        let Some(&[a, b, c, d]) = rest.get(..4) else { return (frames, Ended::Waiting(rest.len())) };
        let len = u32::from_le_bytes([a, b, c, d]);
        if len > LONGEST {
            return (frames, Ended::Broken(format!("a frame of {len} bytes, past the most of {LONGEST}")));
        }
        let Some(body) = rest.get(4..4 + len as usize) else { return (frames, Ended::Waiting(rest.len())) };
        match decode(body) {
            Ok(frame) => frames.push(frame),
            Err(why) => return (frames, Ended::Broken(why)),
        }
        at += 4 + body.len();
    }
}

/// What `Frames::needed` must say when `waiting` are the bytes of the frame not yet whole.
fn needed(waiting: &[u8]) -> usize {
    match waiting.get(..4) {
        Some(&[a, b, c, d]) => 4 + u32::from_le_bytes([a, b, c, d]).min(LONGEST) as usize - waiting.len(),
        _ => 4 - waiting.len(),
    }
}

/// Every frame `reader` has, then how it ends if it broke.
fn drain(reader: &mut Frames, frames: &mut Vec<Frame>) -> Result<usize, String> {
    let mut popped = 0;
    loop {
        match reader.pop()? {
            Some(frame) => frames.push(frame),
            None => return Ok(popped),
        }
        popped += 1;
    }
}

/// How the pieces of a stream are cut.
#[derive(Clone, Copy, Debug)]
enum Pieces<'a> {
    /// Of these sizes in turn.
    Sizes(&'a [usize]),
    /// Each of what `needed` asks for.
    Needed,
}

/// `stream` pushed in `pieces`, every other one by `push_owned` when `owned`, every frame
/// popped after each push.
fn cut(stream: &[u8], pieces: Pieces<'_>, owned: bool) -> (Vec<Frame>, Ended) {
    let mut reader = Frames::new();
    let mut frames = Vec::new();
    let mut at = 0;
    let mut piece = 0;
    loop {
        let waiting = reader.waiting();
        let asked = reader.needed();
        assert_eq!(asked, needed(&stream[at - waiting..at]), "`needed` says what the frame lacks ({pieces:?})");
        assert!(asked > 0 && asked <= 4 + LONGEST as usize, "`needed` of {asked} is past what a frame can need");
        if at == stream.len() {
            return (frames, Ended::Waiting(waiting));
        }
        let size = match pieces {
            Pieces::Sizes(sizes) => sizes[piece % sizes.len()],
            Pieces::Needed => asked,
        };
        let take = size.min(stream.len() - at);
        let bytes = &stream[at..at + take];
        match owned && piece % 2 == 1 {
            true => reader.push_owned(bytes.to_vec()),
            false => reader.push(bytes),
        }
        piece += 1;
        at += take;
        let popped = match drain(&mut reader, &mut frames) {
            Ok(popped) => popped,
            Err(why) => return (frames, Ended::Broken(why)),
        };
        if waiting >= 4 && take == asked {
            assert!(popped > 0, "a push of what `needed` asked for makes the frame whole ({pieces:?})");
        }
    }
}

/// The cuts every stream is pushed in, each with whether every other piece goes in by
/// `push_owned`; the input's own pieces are one more when it has any. One a run beside the
/// push whole (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CUTS: [(Pieces<'static>, bool); 5] = [
    (Pieces::Sizes(&[1]), false),
    (Pieces::Needed, false),
    (Pieces::Needed, true),
    (Pieces::Sizes(&[3, 1, 4, 1, 5, 9, 2, 6]), true),
    (Pieces::Sizes(&[4096, 7]), false),
];

fn check(data: &[u8]) {
    let own = data.split_first().is_some_and(|(head, rest)| head & 7 != 0 && !rest.is_empty());
    check_case(data, blaze_fuzzer::case(data, CUTS.len() + usize::from(own)));
}

fn check_case(data: &[u8], case: usize) {
    let (head, rest) = data.split_first().map_or((0, &[][..]), |(head, rest)| (*head, rest));
    let count = usize::from(head & 7).min(rest.len());
    let (sizes, stream) = rest.split_at(count);
    let sizes: Vec<usize> = sizes.iter().map(|it| usize::from(*it) + 1).collect();
    let owned = head & 8 != 0;

    let (expected, ended) = walk(stream);

    let (most, whole) = blaze_fuzz::largest_allocation(|| {
        let mut reader = Frames::new();
        reader.push(stream);
        let mut frames = Vec::new();
        let ended = match drain(&mut reader, &mut frames) {
            Ok(_) => Ended::Waiting(reader.waiting()),
            Err(why) => Ended::Broken(why),
        };
        (frames, ended)
    });
    assert!(most <= blaze_fuzz::allowed(stream.len()), "reading {} bytes asked for {most} at once", stream.len());
    assert_eq!(whole.0, expected, "the frames pushed whole are the stream's");
    assert_eq!(whole.1, ended, "the stream pushed whole ends as a walk of it does");

    let chosen = match CUTS.get(case) {
        Some(&cut) => Some(cut),
        None => (!sizes.is_empty()).then_some((Pieces::Sizes(&sizes), owned)),
    };
    if let Some((pieces, owned)) = chosen {
        let got = cut(stream, pieces, owned);
        assert_eq!(got.0, expected, "the frames pushed in {pieces:?} (owned: {owned}) are the stream's");
        assert_eq!(got.1, ended, "the stream pushed in {pieces:?} (owned: {owned}) ends as it does whole");
    }

    // The input as one body: whatever it decodes to, it is that frame's bytes.
    _ = decode(data);
}

/// Frames as the server and blaze write them, cut in the ways the first byte says, and the
/// ways a stream is not this protocol's.
fn seeds() -> Vec<Vec<u8>> {
    let frames = [
        Frame::Hello {
            version: 1,
            pid: 42,
            fork: 85_000,
            tests: vec!["a::b".into(), "ünï::cødé".into(), String::new()],
        },
        Frame::Ended {
            id: 7,
            end: End::Exited(0),
            output: b"ok\n".to_vec(),
            executed: Executed::Hits { slots: 64, hits: vec![0, 1, 9, 63] },
        },
        Frame::Ended {
            id: u64::MAX,
            end: End::Exited(-1),
            output: Vec::new(),
            executed: Executed::Hits { slots: 0, hits: Vec::new() },
        },
        Frame::Ended {
            id: 8,
            end: End::Signaled(9),
            output: vec![0, 255, 10],
            executed: Executed::Not("the child was killed".into()),
        },
        Frame::Ended {
            id: 9,
            end: End::NotStarted("fork: no memory".into()),
            output: Vec::new(),
            executed: Executed::Not("the test did not start".into()),
        },
        Frame::Run { id: 3, index: 2 },
        Frame::Kill { id: 3 },
        Frame::Quit,
        Frame::Replicate { count: 4 },
    ];
    let mut stream = Vec::new();
    for _ in 0..2 {
        for frame in &frames {
            frame.encode(&mut stream);
        }
    }
    let with = |head: &[u8], stream: &[u8]| [head, stream].concat();
    let mut torn = stream.clone();
    torn.truncate(torn.len() - 3);
    let mut long = stream.clone();
    long.extend((LONGEST + 1).to_le_bytes());
    long.extend([0; 16]);
    let mut unknown = stream.clone();
    unknown.extend([1, 0, 0, 0, 77]);
    Frame::Quit.encode(&mut unknown);
    vec![
        with(&[0], &stream),
        with(&[0x0b, 0, 6, 63], &stream),
        with(&[0x09, 4], &torn),
        with(&[0x02, 1, 255], &long),
        with(&[0], &unknown),
        body_of(&frames[0]),
    ]
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
        blaze_fuzzer::every_case(&super::TARGET, super::CUTS.len() + 1, super::check_case);
    }
}
