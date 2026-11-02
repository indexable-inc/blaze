use std::time::Duration;

use blaze_ids::rng::SplitMix64;

use super::{
    Executed, Failure, Finished, Outcome, Reported, Trace, fork_server, report,
    wire::{
        End, ExecView, Frame, Frames, HEADER, Hits, LONGEST, MAGIC, UNRECORDED, VERSION, View, closed_ranges, encode_ended,
        encode_hello, header, read_region,
    },
};

fn frames() -> Vec<Frame> {
    vec![
        Frame::Hello { version: VERSION, pid: 42, fork: 51_000, tests: vec!["a::b".into(), "ünï::cødé".into(), String::new()] },
        Frame::Ended { id: 7, end: End::Exited(0), output: b"ok\n".to_vec(), executed: Executed::Hits { slots: 10, hits: vec![0, 3, 9] } },
        Frame::Ended { id: u64::MAX, end: End::Exited(-1), output: Vec::new(), executed: Executed::Hits { slots: 0, hits: Vec::new() } },
        Frame::Ended { id: 8, end: End::Signaled(9), output: vec![0, 255, 10], executed: Executed::Not("it was killed".into()) },
        Frame::Ended {
            id: 9,
            end: End::NotStarted("fork: no memory".into()),
            output: Vec::new(),
            executed: Executed::Not(UNRECORDED.into()),
        },
        Frame::Run { id: 3, index: 2 },
        Frame::Kill { id: 3 },
        Frame::Quit,
        Frame::Replicate { count: 63 },
    ]
}

/// A frame of seeded parts: every kind, ends and records of every kind, empty and long
/// strings and outputs, ids and counts at their edges.
fn seeded(rng: &mut SplitMix64) -> Frame {
    let mut text = |rng: &mut SplitMix64| -> String {
        let len = rng.index(9);
        (0..len).map(|_| ['a', 'é', ':', '\t', '\0', '漢'][rng.index(6)]).collect()
    };
    let edge = |rng: &mut SplitMix64| match rng.below(4) {
        0 => 0,
        1 => u64::MAX,
        _ => rng.next_u64(),
    };
    match rng.below(6) {
        0 => {
            let count = rng.index(5);
            let (version, pid, fork) = (rng.next_u32(), rng.next_u32(), edge(rng));
            Frame::Hello { version, pid, fork, tests: (0..count).map(|_| text(rng)).collect() }
        }
        1 => {
            let end = match rng.below(3) {
                0 => End::Exited(rng.next_u64() as i32),
                1 => End::Signaled(rng.next_u64() as i32),
                _ => End::NotStarted(text(rng)),
            };
            let output = (0..rng.below(300)).map(|_| rng.next_u64() as u8).collect();
            let executed = match rng.below(2) {
                0 => {
                    let slots = rng.below(200) as u32;
                    let hits = (0..slots).filter(|_| rng.one_in(4)).collect();
                    Executed::Hits { slots, hits }
                }
                _ => Executed::Not(text(rng)),
            };
            Frame::Ended { id: edge(rng), end, output, executed }
        }
        2 => Frame::Run { id: edge(rng), index: rng.next_u32() },
        3 => Frame::Kill { id: edge(rng) },
        4 => Frame::Replicate { count: rng.next_u32() },
        _ => Frame::Quit,
    }
}

/// The parts of an end, borrowed, with the hits as the wire has them.
fn ended_parts(frame: &Frame) -> Option<(u64, &End, &[u8], &Executed, Vec<u8>)> {
    let Frame::Ended { id, end, output, executed } = frame else { return None };
    let bytes = match executed {
        Executed::Hits { hits, .. } => hits.iter().flat_map(|it| it.to_le_bytes()).collect(),
        Executed::Not(_) => Vec::new(),
    };
    Some((*id, end, output, executed, bytes))
}

fn view_of<'a>(executed: &'a Executed, bytes: &'a [u8]) -> ExecView<'a> {
    match executed {
        Executed::Hits { slots, .. } => ExecView::Hits { slots: *slots, hits: Hits::of_bytes(bytes).unwrap() },
        Executed::Not(why) => ExecView::Not(why),
    }
}

#[test]
fn every_frame_round_trips() {
    let mut rng = SplitMix64::new(0x853c_49e6_748f_ea9b);
    let seeded: Vec<Frame> = (0..2_000).map(|_| seeded(&mut rng)).collect();
    for frame in frames().into_iter().chain(seeded) {
        let bytes = frame.to_bytes();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4, "{frame:?}");
        assert_eq!(Frame::decode(&bytes[4..]).as_ref(), Ok(&frame));
        let view = View::decode(&bytes[4..]).unwrap();
        assert_eq!(view.to_frame(), frame);
        if let (View::Hello { tests, .. }, Frame::Hello { tests: names, .. }) = (view, &frame) {
            assert_eq!(tests.len(), names.len());
            assert!(tests.iter().eq(names.iter().map(String::as_str)));
        }
    }
}

/// A server's hello from borrowed names is the bytes of the owned frame, and decodes back to it.
#[test]
fn a_hello_from_borrowed_names_is_the_frame() {
    for names in [&[][..], &["a"][..], &["a::b", "", "ünï::cødé", "x"][..]] {
        let frame = Frame::Hello { version: VERSION, pid: 9, fork: 48_500, tests: names.iter().map(|it| (*it).to_owned()).collect() };
        let mut out = b"before".to_vec();
        encode_hello(&mut out, VERSION, 9, 48_500, names.iter().copied());
        assert_eq!(&out[..6], b"before");
        assert_eq!(out[6..], frame.to_bytes()[..]);
        assert_eq!(Frame::decode(&out[10..]), Ok(frame));
    }
}

#[test]
fn frames_come_whole_whatever_the_pieces() {
    let mut stream = Vec::new();
    for frame in frames() {
        frame.encode(&mut stream);
    }
    for piece in [1, 2, 3, 5, 7, 64, stream.len()] {
        let mut reader = Frames::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(piece) {
            reader.push(chunk);
            while let Some(frame) = reader.pop().unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, frames(), "pieces of {piece}");
        assert_eq!(reader.waiting(), 0);
    }
}

#[test]
fn an_end_encoded_from_borrowed_parts_is_the_frame_s_bytes() {
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    let seeded: Vec<Frame> = (0..2_000).map(|_| seeded(&mut rng)).collect();
    for frame in frames().iter().chain(&seeded) {
        let Some((id, end, output, executed, bytes)) = ended_parts(frame) else { continue };
        let mut borrowed = b"before".to_vec();
        encode_ended(&mut borrowed, id, end.view(), output, view_of(executed, &bytes));
        let mut owned = b"before".to_vec();
        frame.encode(&mut owned);
        assert_eq!(borrowed, owned, "{frame:?}");
    }
}

/// Encoding into a buffer that is cleared and reused, and reading a steady stream through one
/// [`Frames`], grow nothing once the largest frame has been seen.
#[test]
fn a_steady_stream_reuses_its_buffers() {
    let frames = frames();
    let mut out = Vec::new();
    let mut reader = Frames::new();
    let mut high = None;
    for round in 0..200 {
        for frame in &frames {
            out.clear();
            match ended_parts(frame) {
                Some((id, end, output, executed, bytes)) => encode_ended(&mut out, id, end.view(), output, view_of(executed, &bytes)),
                None => frame.encode(&mut out),
            }
            for piece in out.chunks(5) {
                reader.push(piece);
                while let Some(view) = reader.pop_view().unwrap() {
                    assert_eq!(&view.to_frame(), frame);
                }
            }
        }
        let now = (out.capacity(), reader.capacity());
        if round == 1 {
            high = Some(now);
        }
        if let Some(high) = high {
            assert_eq!(now, high, "round {round}");
        }
    }
}

/// [`Frames`], which skips over what it read and takes owned pieces whole, against the plain
/// reading of a stream (all of it, then frame after frame), over seeded piece sizes; and
/// what it says it still needs is exactly what makes the next frame whole.
#[test]
fn frames_read_in_place_as_a_whole_stream_reads() {
    let mut stream = Vec::new();
    for _ in 0..5 {
        for frame in frames() {
            frame.encode(&mut stream);
        }
    }
    let mut expected = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let len = u32::from_le_bytes(stream[at..at + 4].try_into().unwrap()) as usize;
        expected.push(Frame::decode(&stream[at + 4..at + 4 + len]).unwrap());
        at += 4 + len;
    }
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for _ in 0..200 {
        let mut reader = Frames::new();
        let mut got = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let state = rng.next_u64();
            let needed = reader.needed();
            let had_length = reader.waiting() >= 4;
            assert!(needed > 0);
            let take = match state % 3 {
                0 => needed,
                _ => 1 + (state >> 8) as usize % 40,
            }
            .min(stream.len() - at);
            let piece = &stream[at..at + take];
            match state % 2 {
                0 => reader.push(piece),
                _ => reader.push_owned(piece.to_vec()),
            }
            at += take;
            let mut popped = 0;
            while let Some(frame) = reader.pop().unwrap() {
                got.push(frame);
                popped += 1;
            }
            // What it needed, once it had the length, made the frame whole.
            if had_length && take == needed {
                assert!(popped > 0);
            }
        }
        assert_eq!(got, expected);
        assert_eq!(reader.waiting(), 0);
        assert_eq!(reader.needed(), 4);
    }
}

#[test]
fn a_frame_that_is_not_one_is_refused() {
    // Too long to be one: refused before its bytes come.
    let mut reader = Frames::new();
    reader.push(&(LONGEST + 1).to_le_bytes());
    assert!(reader.pop().is_err());
    // An unknown kind, bytes after the end, a body that ends early, a string not UTF-8.
    assert!(Frame::decode(&[77]).is_err());
    let mut quit = Frame::Quit.to_bytes()[4..].to_vec();
    quit.push(0);
    assert!(Frame::decode(&quit).is_err());
    let run = Frame::Run { id: 1, index: 2 }.to_bytes();
    assert_eq!(Frame::decode(&run[4..run.len() - 1]).unwrap_err(), "a frame ends early, in its test index");
    let mut hello = Frame::Hello { version: 1, pid: 1, fork: 0, tests: vec!["ab".into()] }.to_bytes()[4..].to_vec();
    let at = hello.len() - 1;
    hello[at] = 0xff;
    assert_eq!(Frame::decode(&hello).unwrap_err(), "the test name of a frame is not UTF-8");
    assert_eq!(Frame::decode(&[]).unwrap_err(), "a frame ends early, in its kind");
    // A count past what the body holds ends early, never allocates for it.
    let mut lying = vec![1];
    lying.extend_from_slice(&1u32.to_le_bytes());
    lying.extend_from_slice(&1u32.to_le_bytes());
    lying.extend_from_slice(&0u64.to_le_bytes());
    lying.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Frame::decode(&lying).unwrap_err(), "a frame ends early, in its test name");
}

/// An end's record must be what a region reader makes: hits strictly increasing, each below
/// the slots, and as many as it says.
#[test]
fn an_execution_record_out_of_order_or_past_its_slots_is_refused() {
    let ended = |slots: u32, hits: Vec<u32>| Frame::Ended { id: 1, end: End::Exited(0), output: Vec::new(), executed: Executed::Hits { slots, hits } };
    for (slots, hits) in [(10, vec![3, 3]), (10, vec![4, 2]), (10, vec![10]), (0, vec![0]), (u32::MAX, vec![u32::MAX])] {
        let bytes = ended(slots, hits.clone()).to_bytes();
        let why = Frame::decode(&bytes[4..]).unwrap_err();
        assert!(why.contains("out of order or past"), "{slots} {hits:?}: {why}");
    }
    assert!(Frame::decode(&ended(u32::MAX, vec![0, u32::MAX - 1]).to_bytes()[4..]).is_ok());
    // A count past the hits there: ends early.
    let mut short = ended(10, vec![1, 2]).to_bytes()[4..].to_vec();
    short.truncate(short.len() - 1);
    assert_eq!(Frame::decode(&short).unwrap_err(), "a frame ends early, in its slots hit");
    let mut huge = ended(10, Vec::new()).to_bytes()[4..].to_vec();
    let at = huge.len() - 4;
    huge[at..].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Frame::decode(&huge).is_err());
    // An unknown kind of record.
    let mut kind = ended(10, Vec::new()).to_bytes()[4..].to_vec();
    let at = kind.len() - 9;
    kind[at] = 2;
    assert!(Frame::decode(&kind).unwrap_err().contains("no execution record is of kind 2"));
}

/// A region as a server maps it: the header, then a byte per slot, padded to whole words.
fn region(slots: u32, set: &[u32], armed: u32) -> Vec<u8> {
    let mut bytes = header(slots).to_vec();
    bytes[12..16].copy_from_slice(&armed.to_le_bytes());
    bytes.resize(HEADER + (slots as usize).div_ceil(8) * 8, 0);
    for &slot in set {
        bytes[HEADER + slot as usize] = 1;
    }
    bytes
}

fn read(bytes: &[u8], mapped: u32, ended: Result<(), &str>, hits: &mut Vec<u8>) -> Result<u32, String> {
    let head: [u8; HEADER] = bytes[..HEADER].try_into().unwrap();
    read_region(head, mapped, ended, |n| u64::from_le_bytes(bytes[HEADER + 8 * n..HEADER + 8 * n + 8].try_into().unwrap()), hits)
}

/// Each refusal of `blaze_testsel::record::Record::of_region`, in its order: not exited,
/// not blaze's header, not armed whole, more slots than mapped. A refusal leaves no hits.
#[test]
fn a_region_that_is_not_a_whole_record_is_refused() {
    let mut hits = vec![9, 9, 9];
    let whole = region(20, &[1, 19], 20);
    let why = read(&whole, 20, Err("it was killed by signal 9"), &mut hits).unwrap_err();
    assert!(why.contains("not whole") && why.contains("signal 9"), "{why}");
    assert!(hits.is_empty());

    let mut bad = whole.clone();
    bad[0] ^= 1;
    assert!(read(&bad, 20, Ok(()), &mut hits).unwrap_err().contains("magic"));
    // A bad magic is said before the arming, as `of_region` checks it first.
    let mut both = region(20, &[], 0);
    both[..8].copy_from_slice(b"notblaze");
    assert!(read(&both, 20, Ok(()), &mut hits).unwrap_err().contains("magic"));

    let unarmed = region(20, &[1], 0);
    assert!(read(&unarmed, 20, Ok(()), &mut hits).unwrap_err().contains("armed 0 of its 20"));
    let partly = region(20, &[1], 19);
    assert!(read(&partly, 20, Ok(()), &mut hits).unwrap_err().contains("armed 19 of its 20"));

    let mut grown = header(30).to_vec();
    grown[12..16].copy_from_slice(&30u32.to_le_bytes());
    grown.resize(HEADER + 24, 0);
    assert!(read(&grown, 20, Ok(()), &mut hits).unwrap_err().contains("fewer than its 30"));
    assert!(hits.is_empty());

    assert_eq!(read(&whole, 20, Ok(()), &mut hits), Ok(20));
    assert_eq!(Hits::of_bytes(&hits).unwrap().to_vec(), [1, 19]);
    assert_eq!(MAGIC, *b"blzexec1");
}

/// The word-at-a-time reader against a byte-at-a-time one, over seeded regions: any non-zero
/// byte is a hit (a runtime may write any), bytes past the last slot in its word are never
/// read as slots, and the hits are what the wire then accepts.
#[test]
fn a_region_reads_as_its_bytes_one_by_one() {
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut hits = Vec::new();
    for _ in 0..3_000 {
        let slots = rng.below(300) as u32;
        let density = 1 + rng.below(16);
        let mut bytes = region(slots, &[], slots);
        for byte in &mut bytes[HEADER..] {
            if rng.one_in(density) {
                *byte = rng.next_u64() as u8 | 1;
            }
        }
        let expected: Vec<u32> = (0..slots).filter(|&slot| bytes[HEADER + slot as usize] != 0).collect();
        let mapped = slots + rng.below(3) as u32;
        assert_eq!(read(&bytes, mapped, Ok(()), &mut hits), Ok(slots));
        let got = Hits::of_bytes(&hits).unwrap();
        assert_eq!(got.to_vec(), expected, "{slots} slots");
        assert_eq!(got.len(), expected.len());
        let mut out = Vec::new();
        encode_ended(&mut out, 1, End::Exited(0).view(), &[], ExecView::Hits { slots, hits: got });
        let Ok(Frame::Ended { executed, .. }) = Frame::decode(&out[4..]) else { panic!("{slots}") };
        assert_eq!(executed, Executed::Hits { slots, hits: expected });
    }
    // Every slot of every count past two groups of four words, alone and with the one after
    // the last set: the groups, the words after the last group and the masked last word.
    for slots in 0..=72u32 {
        for at in 0..slots {
            let mut bytes = region(slots, &[], slots);
            bytes[HEADER + at as usize] = 1;
            if let Some(past) = bytes.get_mut(HEADER + slots as usize) {
                *past = 1;
            }
            assert_eq!(read(&bytes, slots, Ok(()), &mut hits), Ok(slots));
            assert_eq!(Hits::of_bytes(&hits).unwrap().to_vec(), [at], "{slots} slots");
        }
    }
    // Every byte of every value, alone in its word.
    for at in 0..8u32 {
        for value in 1..=255u8 {
            let mut bytes = region(8, &[], 8);
            bytes[HEADER + at as usize] = value;
            assert_eq!(read(&bytes, 8, Ok(()), &mut hits), Ok(8));
            assert_eq!(Hits::of_bytes(&hits).unwrap().to_vec(), [at]);
        }
    }
}

#[test]
fn a_child_closes_exactly_what_was_not_open() {
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let (mut open, mut ranges) = (Vec::new(), Vec::new());
    for _ in 0..500 {
        open.clear();
        let count = rng.below(12);
        for _ in 0..count {
            open.push(rng.below(40) as u32);
        }
        let model: Vec<u32> = open.clone();
        closed_ranges(&mut open, &mut ranges);
        assert_eq!(ranges.last().map(|it| it.1), Some(u32::MAX));
        assert!(ranges.windows(2).all(|it| it[0].1 < it[1].0), "{ranges:?}");
        assert!(ranges.iter().all(|it| it.0 >= 3 && it.0 <= it.1), "{ranges:?}");
        for fd in 0..64u32 {
            let closed = ranges.iter().any(|&(first, last)| first <= fd && fd <= last);
            assert_eq!(closed, fd >= 3 && !model.contains(&fd), "{fd} with {model:?}: {ranges:?}");
        }
    }
    open.clear();
    closed_ranges(&mut open, &mut ranges);
    assert_eq!(ranges, [(3, u32::MAX)], "nothing but 0 to 2 open: one range");
}

#[test]
fn the_harness_texts_are_one_module_with_the_protocol_and_the_server() {
    for record in [true, false] {
        let text = fork_server(record);
        assert!(std::ptr::eq(text, fork_server(record)), "made once");
        let opened = text.trim_start().trim_start_matches("#[allow(dead_code)]").trim_start();
        assert!(opened.starts_with("pub mod fork_server {"));
        assert!(text.contains("pub const SOCKET: &str = \"__BLAZE_FORK_SERVER\";"));
        assert!(text.contains("pub const VERSION: u32 = 3;"));
        // The server measures its fork for blaze, and makes replicas only when asked.
        assert!(text.contains("let fork = fork_cost()?;"));
        assert!(text.contains("View::Replicate { count } =>"));
        assert!(text.contains("exits::forked(&mut queue)?;"));
        // Both kinds of record take a region back for the next test.
        assert!(text.contains("pub fn map(&mut self) -> Result<Region, String>"));
        assert!(text.contains("pub fn give(&mut self, "));
        assert!(text.contains("exec.give(child.region, &hits, read.is_ok());"));
        // A test's child closes the server's descriptors in one call per gap (Linux).
        assert!(text.contains("close_range"));
        assert!(text.contains("Kept::now()"));
        assert!(text.contains("pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> !"));
        // Nothing of blaze is named in what the tested crate compiles but the runtime's
        // exports, and those only with slots.
        let code: Vec<&str> = text.lines().filter(|it| !it.trim_start().starts_with("//")).collect();
        let named = |it: &str| it.replace("__blaze_exec_slots", "").replace("__blaze_exec_arm", "");
        assert!(code.iter().all(|it| !it.contains("crate::") && !named(it).contains("blaze_")), "{code:#?}");
        assert_eq!(code.iter().any(|it| it.contains("fn __blaze_exec_slots() -> u32;")), record);
        assert_eq!(code.iter().any(|it| it.contains("fn __blaze_exec_arm(")), record);
        assert_eq!(code.iter().any(|it| it.contains("MAP_SHARED | MAP_ANONYMOUS")), record);
        // Nothing an older edition reads otherwise.
        assert!(code.iter().all(|it| !it.contains("'_")), "{code:#?}");
        // An inner doc comment only where a module starts: the protocol's, first.
        let after_open = opened.trim_start_matches("pub mod fork_server {").trim_start();
        assert!(after_open.starts_with("//!"));
        let past_docs: String = after_open.lines().skip_while(|it| it.starts_with("//!")).collect::<Vec<_>>().join("\n");
        assert!(!past_docs.contains("//!"));
        // Every unsafe block says why it is sound.
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            if line.contains("unsafe {") || line.trim_start().starts_with("unsafe {") {
                let before = lines[..at].iter().rev().take(12).any(|it| it.contains("SAFETY:"));
                assert!(before, "an unsafe block with no SAFETY comment above it: {line}");
            }
        }
        // A child's end wakes the server (a pidfd, or a kqueue): it never waits on a clock.
        assert!(text.contains("const PIDFD_OPEN: ::core::ffi::c_long = 434;"));
        assert!(text.contains("fn kqueue() -> Int;"));
        assert!(text.contains("let timeout = if now { 0 } else { -1 };"));
        // No test of blaze's in it, which the tested crate's harness would run as its own.
        assert!(!text.contains("#[test]"));
        assert!(!text.contains("#[cfg(test)]"));
        let (open, close) = text.chars().fold((0, 0), |(open, close), it| match it {
            '{' => (open + 1, close),
            '}' => (open, close + 1),
            _ => (open, close),
        });
        assert_eq!(open, close);
    }
    assert!(fork_server(false).contains(UNRECORDED));
}

fn finished(outcome: Outcome) -> Finished {
    Finished {
        binary: 0,
        test: Some("a::b".into()),
        outcome,
        attempts: 1,
        output: b"out".to_vec(),
        took: Duration::from_millis(1500),
        trace: Trace::Unknown("not traced".into()),
        executed: Executed::Not(UNRECORDED.into()),
    }
}

#[test]
fn results_are_reported_as_the_api_has_them() {
    let passed = report("lib", &finished(Outcome::Passed)).unwrap();
    assert_eq!((passed.outcome, passed.millis, passed.name.as_str()), (Reported::Passed, 1500, "a::b"));
    assert_eq!(passed.output, b"out");
    let timed_out = report("lib", &finished(Outcome::Failed(Failure::TimedOut(Duration::from_secs(60))))).unwrap();
    assert_eq!(timed_out.outcome, Reported::Failed);
    assert!(String::from_utf8_lossy(&timed_out.output).contains("timeout"));
    let code = report("lib", &finished(Outcome::Failed(Failure::Code(1)))).unwrap();
    assert_eq!(code.output, b"out");
    assert_eq!(report("lib", &finished(Outcome::NotRun)), None);
    let mut whole = finished(Outcome::Passed);
    whole.test = None;
    assert_eq!(report("lib", &whole).unwrap().name, "");
}

/// Once its buffers have grown, the wire allocates nothing: a frame encoded into a reused
/// buffer, pushed and read in place, a region read into reused hits, an end encoded.
#[test]
fn the_wire_allocates_nothing_once_its_buffers_have_grown() {
    let frames = frames();
    let bytes = region(300, &[0, 7, 64, 299], 300);
    let (mut out, mut stream, mut hits) = (Vec::new(), Frames::new(), Vec::new());
    let mut round = || {
        for frame in &frames {
            out.clear();
            frame.encode(&mut out);
            stream.push(&out);
            while let Some(view) = stream.pop_view().unwrap() {
                std::hint::black_box(&view);
            }
        }
        assert_eq!(read(&bytes, 300, Ok(()), &mut hits), Ok(300));
        out.clear();
        encode_ended(&mut out, 1, End::Exited(0).view(), b"ok", ExecView::Hits { slots: 300, hits: Hits::of_bytes(&hits).unwrap() });
    };
    round();
    blaze_alloc_count::assert_at_most(0, "frames encoded and read in place, a region read, an end encoded", &mut round);
}
